//! Linux systemd --user backend for ProcessManager.
//!
//! Assumes:
//! - ClawOps runs as root (or a user with sudo/NOPASSWD on useradd/loginctl/systemctl).
//! - A systemd user-template unit `zeroclaw@.service` is installed system-wide
//!   under `/etc/systemd/user/zeroclaw@.service` (see installer script).

use super::{ProcessManager, ProcessStatus, UserHomeLayout};
use crate::{Error, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
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

async fn systemctl_user(linux_uid: &str, args: &[&str]) -> Result<String> {
    // Run `sudo -u <uid> XDG_RUNTIME_DIR=/run/user/$(id -u <uid>) systemctl --user ...`
    let mut full = vec!["-u", linux_uid, "--", "systemctl", "--user"];
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
            run("loginctl", &["enable-linger", linux_uid]).await?;
        }
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
        Ok(())
    }

    async fn start(&self, linux_uid: &str) -> Result<()> {
        systemctl_user(linux_uid, &["start", &format!("zeroclaw@{linux_uid}")]).await?;
        Ok(())
    }

    async fn stop(&self, linux_uid: &str) -> Result<()> {
        systemctl_user(linux_uid, &["stop", &format!("zeroclaw@{linux_uid}")]).await?;
        Ok(())
    }

    async fn status(&self, linux_uid: &str) -> Result<ProcessStatus> {
        let unit = format!("zeroclaw@{linux_uid}");
        let out =
            Command::new("sudo")
                .args(["-u", linux_uid, "--", "systemctl", "--user", "is-active", &unit])
                .output()
                .await?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(match text.as_str() {
            "active" => ProcessStatus::Running,
            "inactive" | "failed" => ProcessStatus::Stopped,
            _ => ProcessStatus::Unknown,
        })
    }
}
