use crate::Result;
use async_trait::async_trait;
use std::path::PathBuf;

pub mod mock;
#[cfg(target_os = "linux")]
pub mod systemd;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Stopped,
    Failed(String),
    Unknown,
}

pub struct UserHomeLayout {
    pub home_dir: PathBuf,       // e.g. /home/claw-001 or /tmp/clawops-dev/claw-001
    pub config_dir: PathBuf,     // <home>/.zeroclaw
    pub workspace_dir: PathBuf,  // <home>/.zeroclaw/workspace
}

impl UserHomeLayout {
    pub fn new(home_base: &std::path::Path, linux_uid: &str) -> Self {
        let home_dir = home_base.join(linux_uid);
        let config_dir = home_dir.join(".zeroclaw");
        let workspace_dir = config_dir.join("workspace");
        Self {
            home_dir,
            config_dir,
            workspace_dir,
        }
    }
}

#[async_trait]
pub trait ProcessManager: Send + Sync {
    /// Whether this backend really launches a zeroclaw daemon (true) or just
    /// records intent (false, mock). Provisioner uses this to decide whether to
    /// wait on /health and /pair.
    fn launches_daemon(&self) -> bool;

    /// Create the OS-level user account and home directory skeleton if needed.
    async fn ensure_linux_user(&self, linux_uid: &str, layout: &UserHomeLayout) -> Result<()>;

    /// Chown workspace to the user (systemd only). Mock impl is a no-op.
    async fn chown_workspace(&self, linux_uid: &str, layout: &UserHomeLayout) -> Result<()>;

    /// Start zeroclaw daemon for this user.
    async fn start(&self, linux_uid: &str) -> Result<()>;

    /// Stop zeroclaw daemon for this user (idempotent).
    async fn stop(&self, linux_uid: &str) -> Result<()>;

    /// Probe whether the daemon is currently running.
    async fn status(&self, linux_uid: &str) -> Result<ProcessStatus>;
}

pub fn make(
    backend: &str,
    zeroclaw_binary: PathBuf,
    home_base: PathBuf,
) -> Result<Box<dyn ProcessManager>> {
    #[cfg(not(target_os = "linux"))]
    let _ = &zeroclaw_binary;
    match backend {
        "mock" => Ok(Box::new(mock::MockProcessManager::new(home_base))),
        #[cfg(target_os = "linux")]
        "systemd" => Ok(Box::new(systemd::SystemdProcessManager::new(
            zeroclaw_binary,
            home_base,
        ))),
        #[cfg(not(target_os = "linux"))]
        "systemd" => Err(crate::Error::Process(
            "systemd backend only available on Linux; use backend = \"mock\" in dev".into(),
        )),
        other => Err(crate::Error::Process(format!(
            "unknown provisioner backend: {other}"
        ))),
    }
}
