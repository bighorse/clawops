//! Linux systemd --user backend for ProcessManager.
//!
//! Assumes:
//! - ClawOps runs as root (or a user with sudo/NOPASSWD on useradd/loginctl/systemctl).
//! - A systemd user-template unit `zeroclaw@.service` is installed system-wide
//!   under `/etc/systemd/user/zeroclaw@.service` (see installer script).
//!
//! systemctl --user requires the target user's D-Bus session, reachable via
//! `XDG_RUNTIME_DIR=/run/user/<uid>`. `loginctl enable-linger` boots the
//! per-user `user@<uid>.service` which creates `/run/user/<uid>/bus`. We wait
//! for that socket before issuing systemctl commands.

use super::{ProcessManager, ProcessStatus, UserHomeLayout};
use crate::{Error, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub struct SystemdProcessManager {
    #[allow(dead_code)]
    zeroclaw_binary: PathBuf,
    #[allow(dead_code)]
    home_base: PathBuf,
}

impl SystemdProcessManager {
    pub fn new(zeroclaw_binary: PathBuf, home_base: PathBuf) -> Self {
        Self {
            zeroclaw_binary,
            home_base,
        }
    }
}

async fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        return Err(Error::Process(format!(
            "{cmd} {:?} exit={} stderr={}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn run_capture(cmd: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
}

async fn lookup_uid(linux_uid: &str) -> Result<u32> {
    let out = run("id", &["-u", linux_uid]).await?;
    out.trim()
        .parse::<u32>()
        .map_err(|e| Error::Process(format!("parse uid for {linux_uid}: {e}")))
}

/// Wait for `/run/user/<uid>/bus` to exist (i.e. the user's D-Bus session is up).
async fn wait_user_bus(numeric_uid: u32, timeout: Duration) -> Result<()> {
    let path = format!("/run/user/{numeric_uid}/bus");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::path::Path::new(&path).exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Process(format!(
                "user bus {path} did not appear within {:?}",
                timeout
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `sudo -u <uid> env XDG_RUNTIME_DIR=/run/user/<numeric_uid> systemctl --user <args>`
async fn systemctl_user(linux_uid: &str, args: &[&str]) -> Result<String> {
    let numeric_uid = lookup_uid(linux_uid).await?;
    let xdg = format!("XDG_RUNTIME_DIR=/run/user/{numeric_uid}");
    let mut full: Vec<&str> = vec![
        "-u",
        linux_uid,
        "env",
        xdg.as_str(),
        "systemctl",
        "--user",
    ];
    full.extend_from_slice(args);
    run("sudo", &full).await
}

#[async_trait]
impl ProcessManager for SystemdProcessManager {
    fn launches_daemon(&self) -> bool {
        true
    }

    async fn ensure_linux_user(&self, linux_uid: &str, layout: &UserHomeLayout) -> Result<()> {
        let home = layout.home_dir.to_string_lossy().to_string();
        // useradd idempotently
        let check = Command::new("id").arg(linux_uid).output().await?;
        if !check.status.success() {
            run(
                "useradd",
                &["-m", "-d", &home, "-s", "/bin/bash", linux_uid],
            )
            .await?;
        }
        // enable-linger is idempotent — safe to call every time.
        run("loginctl", &["enable-linger", linux_uid]).await?;

        // Grant the new uid read access to the shared LLM env file. The file
        // is root:root 0600 by convention; setfacl adds a per-user ACL entry
        // so the user-systemd manager can read it. Idempotent. Best-effort —
        // skip silently if the file is absent (e.g. dev without zeroclaw.env).
        let env_file = "/etc/clawops/zeroclaw.env";
        if std::path::Path::new(env_file).exists() {
            if let Err(e) = run(
                "setfacl",
                &["-m", &format!("u:{linux_uid}:r"), env_file],
            )
            .await
            {
                tracing::warn!(
                    uid = linux_uid,
                    "setfacl on {env_file} failed (non-fatal): {e}"
                );
            }
        }

        // Wait for user@<uid>.service to be ready (bus socket exists).
        let numeric_uid = lookup_uid(linux_uid).await?;
        wait_user_bus(numeric_uid, Duration::from_secs(10)).await?;

        std::fs::create_dir_all(&layout.workspace_dir)?;
        let _ = &self.zeroclaw_binary;
        let _ = &self.home_base;
        Ok(())
    }

    async fn chown_workspace(&self, linux_uid: &str, layout: &UserHomeLayout) -> Result<()> {
        let home = layout.home_dir.to_string_lossy().to_string();
        run(
            "chown",
            &["-R", &format!("{linux_uid}:{linux_uid}"), &home],
        )
        .await?;
        run("chmod", &["700", &home]).await?;
        // config.toml contains the paired_token — restrict to 0600 to silence
        // zeroclaw's "world-readable" warning.
        let cfg_path = layout.config_dir.join("config.toml");
        if cfg_path.exists() {
            run("chmod", &["600", cfg_path.to_string_lossy().as_ref()]).await?;
        }
        Ok(())
    }

    async fn start(&self, linux_uid: &str) -> Result<()> {
        // Reset failed state from previous attempts so restart counter is clean.
        let _ =
            systemctl_user(linux_uid, &["reset-failed", &format!("zeroclaw@{linux_uid}")]).await;
        systemctl_user(linux_uid, &["start", &format!("zeroclaw@{linux_uid}")]).await?;
        Ok(())
    }

    async fn stop(&self, linux_uid: &str) -> Result<()> {
        systemctl_user(linux_uid, &["stop", &format!("zeroclaw@{linux_uid}")]).await?;
        Ok(())
    }

    async fn status(&self, linux_uid: &str) -> Result<ProcessStatus> {
        let numeric_uid = lookup_uid(linux_uid).await?;
        let xdg = format!("XDG_RUNTIME_DIR=/run/user/{numeric_uid}");
        let unit = format!("zeroclaw@{linux_uid}");
        let out = run_capture(
            "sudo",
            &[
                "-u",
                linux_uid,
                "env",
                &xdg,
                "systemctl",
                "--user",
                "is-active",
                &unit,
            ],
        )
        .await?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(match text.as_str() {
            "active" => ProcessStatus::Running,
            "inactive" | "failed" => ProcessStatus::Stopped,
            _ => ProcessStatus::Unknown,
        })
    }
}
