use crate::config::Config;
use crate::process::{ProcessManager, UserHomeLayout};
use crate::{ports, users, Error, Result};
use handlebars::Handlebars;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;

pub struct Provisioner {
    pub pool: SqlitePool,
    pub cfg: Arc<Config>,
    pub backend: Arc<dyn ProcessManager>,
    pub http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct ProvisionOutcome {
    pub openid: String,
    pub linux_uid: String,
    pub port: u16,
    pub workspace_path: String,
    pub paired: bool,
}

impl Provisioner {
    pub async fn provision(&self, new: &users::NewUser) -> Result<ProvisionOutcome> {
        if users::get(&self.pool, &new.openid).await?.is_some() {
            return Err(Error::UserAlreadyExists(new.openid.clone()));
        }

        let linux_uid = users::next_linux_uid(&self.pool).await?;
        let layout = UserHomeLayout::new(&self.cfg.zeroclaw.home_base, &linux_uid);
        let workspace_path = layout.workspace_dir.to_string_lossy().to_string();

        let user = users::insert_provisioning(
            &self.pool,
            new,
            &linux_uid,
            &workspace_path,
        )
        .await?;
        users::log_step(&self.pool, &user.openid, "db_insert", true, None).await?;

        let port = ports::allocate(
            &self.pool,
            &ports::PortRange {
                start: self.cfg.zeroclaw.port_range_start,
                end: self.cfg.zeroclaw.port_range_end,
            },
            &user.openid,
        )
        .await?;
        users::set_port(&self.pool, &user.openid, Some(port)).await?;
        users::log_step(
            &self.pool,
            &user.openid,
            "port_allocate",
            true,
            Some(&port.to_string()),
        )
        .await?;

        if let Err(e) = self.provision_inner(&user, port, &layout).await {
            users::set_error(&self.pool, &user.openid, &e.to_string()).await.ok();
            users::log_step(&self.pool, &user.openid, "fatal", false, Some(&e.to_string()))
                .await
                .ok();
            return Err(e);
        }

        users::set_status(&self.pool, &user.openid, "running").await?;

        Ok(ProvisionOutcome {
            openid: user.openid,
            linux_uid,
            port,
            workspace_path,
            paired: self.backend.launches_daemon(),
        })
    }

    async fn provision_inner(
        &self,
        user: &users::User,
        port: u16,
        layout: &UserHomeLayout,
    ) -> Result<()> {
        self.backend
            .ensure_linux_user(&user.linux_uid, layout)
            .await?;
        users::log_step(&self.pool, &user.openid, "ensure_linux_user", true, None).await?;

        self.render_templates(user, port, layout).await?;
        users::log_step(&self.pool, &user.openid, "render_templates", true, None).await?;

        self.backend
            .chown_workspace(&user.linux_uid, layout)
            .await?;
        users::log_step(&self.pool, &user.openid, "chown", true, None).await?;

        if self.backend.launches_daemon() {
            self.backend.start(&user.linux_uid).await?;
            users::log_step(&self.pool, &user.openid, "systemd_start", true, None).await?;

            self.wait_health(port).await?;
            users::log_step(&self.pool, &user.openid, "health_ok", true, None).await?;

            // Pairing: POST /pair with X-Pairing-Code. The pairing code is
            // written into config.toml under [gateway] pair_code (phase 1
            // convention). zeroclaw upstream reads pair code from its own
            // admin flow; for now we assume require_pairing=false in phase 1
            // templates, so we skip /pair and mark paired=false.
            users::log_step(&self.pool, &user.openid, "pair_skipped_phase1", true, None).await?;
        } else {
            users::log_step(&self.pool, &user.openid, "daemon_skipped_mock", true, None).await?;
        }

        Ok(())
    }

    async fn render_templates(
        &self,
        user: &users::User,
        port: u16,
        layout: &UserHomeLayout,
    ) -> Result<()> {
        let profile: serde_json::Value = user
            .enterprise_profile
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));

        // Generate a strong bearer token and inject it directly into
        // [gateway] paired_tokens. Skips the pair handshake — ClawOps is
        // the only client of each user's zeroclaw, so a pre-shared token
        // is simpler and equivalent in security.
        //
        // The `zc_` prefix is critical: zeroclaw's `is_token_hash()` treats
        // any bare 64-hex string as an *already-hashed* value and stores it
        // verbatim, which means client-supplied plaintext (re-hashed on
        // verification) will never match. The prefix makes the length 67
        // and forces zeroclaw to hash on load instead.
        let paired_token = format!(
            "zc_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        users::set_paired_token(&self.pool, &user.openid, &paired_token).await?;

        let tpl = &self.cfg.zeroclaw_template;
        let ctx = json!({
            "paired_token": paired_token,
            "openid": user.openid,
            "phone": user.phone,
            "display_name": user.display_name,
            "linux_uid": user.linux_uid,
            "port": port,
            "workspace_path": layout.workspace_dir,
            "config_dir": layout.config_dir,
            "home_dir": layout.home_dir,
            "enterprise": profile,
            "llm": {
                "default_provider": tpl.default_provider,
                "default_model": tpl.default_model,
                "api_key": tpl.api_key,
                "api_url": tpl.api_url,
                "default_temperature": tpl.default_temperature,
                "provider_timeout_secs": tpl.provider_timeout_secs,
                "max_cost_per_day_cents": tpl.max_cost_per_day_cents,
            },
        });

        let mut hb = Handlebars::new();
        hb.set_strict_mode(false);

        let tpl_dir = &self.cfg.provisioner.template_dir;
        std::fs::create_dir_all(&layout.workspace_dir)?;
        std::fs::create_dir_all(&layout.config_dir)?;

        render_one(&hb, tpl_dir, "USER.md.hbs", &layout.workspace_dir.join("USER.md"), &ctx)?;
        render_one(&hb, tpl_dir, "IDENTITY.md.hbs", &layout.workspace_dir.join("IDENTITY.md"), &ctx)?;
        render_one(&hb, tpl_dir, "SOUL.md.hbs", &layout.workspace_dir.join("SOUL.md"), &ctx)?;
        render_one(&hb, tpl_dir, "config.toml.hbs", &layout.config_dir.join("config.toml"), &ctx)?;
        Ok(())
    }

    async fn wait_health(&self, port: u16) -> Result<()> {
        let url = format!("http://127.0.0.1:{port}/health");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            match self.http.get(&url).send().await {
                Ok(r) if r.status().is_success() => return Ok(()),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::ZeroclawNotReady {
                    host: "127.0.0.1".into(),
                    port,
                    waited_ms: 20_000,
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn stop(&self, openid: &str) -> Result<()> {
        let user = users::get_required(&self.pool, openid).await?;
        self.backend.stop(&user.linux_uid).await?;
        if let Some(p) = user.port {
            ports::release(&self.pool, p as u16).await?;
            users::set_port(&self.pool, &user.openid, None).await?;
        }
        users::set_status(&self.pool, &user.openid, "stopped").await?;
        Ok(())
    }

    pub async fn ensure_running(&self, openid: &str) -> Result<users::User> {
        let user = users::get_required(&self.pool, openid).await?;
        if user.status == "running" && user.port.is_some() {
            return Ok(user);
        }

        let port = match user.port {
            Some(p) => p as u16,
            None => {
                let p = ports::allocate(
                    &self.pool,
                    &ports::PortRange {
                        start: self.cfg.zeroclaw.port_range_start,
                        end: self.cfg.zeroclaw.port_range_end,
                    },
                    &user.openid,
                )
                .await?;
                users::set_port(&self.pool, &user.openid, Some(p)).await?;
                p
            }
        };

        if self.backend.launches_daemon() {
            self.backend.start(&user.linux_uid).await?;
            self.wait_health(port).await?;
        }
        users::set_status(&self.pool, &user.openid, "running").await?;
        users::get_required(&self.pool, openid).await
    }
}

fn render_one(
    hb: &Handlebars,
    tpl_dir: &std::path::Path,
    tpl_name: &str,
    out_path: &std::path::Path,
    ctx: &serde_json::Value,
) -> Result<()> {
    let tpl_path = tpl_dir.join(tpl_name);
    if !tpl_path.exists() {
        tracing::warn!(
            "template {} missing, skipping",
            tpl_path.display()
        );
        return Ok(());
    }
    let tpl = std::fs::read_to_string(&tpl_path)?;
    let out = hb.render_template(&tpl, ctx)?;
    std::fs::write(out_path, out)?;
    Ok(())
}
