use super::{ProcessManager, ProcessStatus, UserHomeLayout};
use crate::Result;
use async_trait::async_trait;
use std::path::PathBuf;

pub struct MockProcessManager {
    home_base: PathBuf,
}

impl MockProcessManager {
    pub fn new(home_base: PathBuf) -> Self {
        Self { home_base }
    }
}

#[async_trait]
impl ProcessManager for MockProcessManager {
    fn launches_daemon(&self) -> bool {
        false
    }

    async fn ensure_linux_user(&self, linux_uid: &str, layout: &UserHomeLayout) -> Result<()> {
        std::fs::create_dir_all(&layout.workspace_dir)?;
        tracing::info!(
            uid = linux_uid,
            home = %layout.home_dir.display(),
            "mock: ensure_linux_user (created dir, no OS user)"
        );
        let _ = &self.home_base;
        Ok(())
    }

    async fn chown_workspace(&self, linux_uid: &str, _layout: &UserHomeLayout) -> Result<()> {
        tracing::info!(uid = linux_uid, "mock: chown skipped");
        Ok(())
    }

    async fn start(&self, linux_uid: &str) -> Result<()> {
        tracing::info!(uid = linux_uid, "mock: start (no real daemon)");
        Ok(())
    }

    async fn stop(&self, linux_uid: &str) -> Result<()> {
        tracing::info!(uid = linux_uid, "mock: stop");
        Ok(())
    }

    async fn status(&self, _linux_uid: &str) -> Result<ProcessStatus> {
        Ok(ProcessStatus::Unknown)
    }
}
