use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub zeroclaw: ZeroclawConfig,
    pub provisioner: ProvisionerConfig,
    pub zeroclaw_template: ZeroclawTemplateConfig,
    #[serde(default)]
    pub wx: WxConfig,
    #[serde(default)]
    pub reaper: ReaperConfig,
    #[serde(default)]
    pub admin: AdminConfig,
}

/// Admin API protection. The /admin/* routes are gated by a static
/// `X-Admin-Token` header; if `token` is empty the routes return 503
/// (service available but admin disabled). This is **not** a substitute
/// for network-level isolation — operators should still bind 127.0.0.1
/// and front via reverse proxy in production.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AdminConfig {
    #[serde(default)]
    pub token: String,
}

/// LLM/provider settings injected into each rendered per-user `config.toml`.
/// Centralising these in ClawOps avoids duplicating secrets across users.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ZeroclawTemplateConfig {
    pub default_provider: String,
    pub default_model: String,
    /// API key passed to zeroclaw. Prefer empty here + use `ZEROCLAW_API_KEY`
    /// env in the systemd unit so secrets never sit in per-user config.toml.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default = "default_temperature")]
    pub default_temperature: f64,
    #[serde(default = "default_provider_timeout_secs")]
    pub provider_timeout_secs: u64,
    /// Per-user daily cost cap in cents. ClawOps writes this into the
    /// rendered `[autonomy] max_cost_per_day_cents` field.
    #[serde(default = "default_max_cost_per_day_cents")]
    pub max_cost_per_day_cents: u64,
}

fn default_temperature() -> f64 {
    0.7
}
fn default_provider_timeout_secs() -> u64 {
    120
}
fn default_max_cost_per_day_cents() -> u64 {
    500
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ZeroclawConfig {
    pub binary: PathBuf,
    pub home_base: PathBuf,
    pub port_range_start: u16,
    pub port_range_end: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvisionerConfig {
    pub backend: String,
    pub template_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WxConfig {
    #[serde(default)]
    pub appid: String,
    #[serde(default)]
    pub secret: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReaperConfig {
    pub idle_stop_minutes: i64,
    pub idle_archive_minutes: i64,
}

impl Default for ReaperConfig {
    fn default() -> Self {
        Self {
            idle_stop_minutes: 90 * 24 * 60,
            idle_archive_minutes: 365 * 24 * 60,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> crate::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }
}
