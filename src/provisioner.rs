use crate::config::Config;
use crate::process::{ProcessManager, UserHomeLayout};
use crate::{ports, users, Error, Result};
use handlebars::Handlebars;
use reqwest::header;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;

/// Crude URL → host extraction (no `url` crate). Handles
/// "https://host/path?qs" / "http://host:port/" — strips scheme,
/// stops at first slash, drops port. Returns None if input doesn't
/// look like a URL.
fn extract_host(url: &str) -> Option<String> {
    let stripped = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_with_port = stripped.split('/').next()?;
    let host = host_with_port.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Build the TOML-array literal for `[http_request].allowed_domains`
/// based on which integrations are enabled. Always returns a valid
/// TOML array string, even when no hosts are needed (an empty array
/// disables the http_request tool).
fn http_allowed_domains_toml(cfg: &Config) -> String {
    let mut hosts: Vec<String> = Vec::new();
    let push_unique = |hosts: &mut Vec<String>, h: String| {
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    };
    if let Some(h) = extract_host(&cfg.commodity.api_base) {
        push_unique(&mut hosts, h);
    }
    if let Some(h) = extract_host(&cfg.activity.api_base) {
        push_unique(&mut hosts, h);
    }
    if let Some(h) = extract_host(&cfg.policy.api_base) {
        push_unique(&mut hosts, h);
    }
    if let Some(h) = extract_host(&cfg.policy_match.api_base) {
        push_unique(&mut hosts, h);
    }
    // Space service has no enable toggle — its host is always whitelisted.
    if let Some(h) = extract_host(&cfg.space.api_base) {
        push_unique(&mut hosts, h);
    }
    if let Some(h) = extract_host(&cfg.lead.webhook_url) {
        push_unique(&mut hosts, h);
    }
    if !cfg.qualification.api_base.is_empty() {
        if let Some(h) = extract_host(&cfg.qualification.api_base) {
            push_unique(&mut hosts, h);
        }
    }
    let quoted: Vec<String> = hosts.iter().map(|h| format!("\"{h}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

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
    pub breed: String,
}

impl Provisioner {
    pub async fn provision(&self, new: &users::NewUser) -> Result<ProvisionOutcome> {
        if users::get(&self.pool, &new.openid).await?.is_some() {
            return Err(Error::UserAlreadyExists(new.openid.clone()));
        }

        // Resolve the breed first. Anything after this point burns a
        // linux uid and a DB row, so an unknown breed has to be rejected
        // while the swarm is still untouched.
        let breed = new
            .breed
            .clone()
            .unwrap_or_else(|| self.cfg.provisioner.default_breed.clone());
        self.breed_dir(&breed)?;

        let linux_uid = users::next_linux_uid(&self.pool).await?;
        let layout = UserHomeLayout::new(&self.cfg.zeroclaw.home_base, &linux_uid);
        let workspace_path = layout.workspace_dir.to_string_lossy().to_string();

        let user = users::insert_provisioning(
            &self.pool,
            new,
            &linux_uid,
            &workspace_path,
            &breed,
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
            breed,
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

            self.wait_ready(&user.openid, port).await?;
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

    /// Template directory for this tenant's breed. Refuses to fall back:
    /// a tenant whose breed lost its directory (bundle deleted, breeds_dir
    /// unmounted) must fail visibly, not quietly start answering as some
    /// other kind of lobster.
    fn breed_dir(&self, breed: &str) -> Result<std::path::PathBuf> {
        self.cfg
            .provisioner
            .breed_dir(breed)
            .ok_or_else(|| Error::UnknownBreed(breed.to_string()))
    }

    /// Everything the handlebars templates can read. Built once and shared
    /// by provision and refresh — when the two drifted apart, a field added
    /// for provisioning silently rendered empty on every later refresh.
    /// A context shaped exactly like a real tenant's, for rendering a breed
    /// at install time so it can be inspected before anyone is put on it.
    ///
    /// Deliberately goes through `build_ctx` rather than hand-rolling a
    /// lookalike: the point is to see what tenants will see, and a second
    /// implementation would drift from the first exactly when it matters.
    pub fn probe_ctx(&self, breed: &str) -> serde_json::Value {
        let user = users::User {
            openid: "probe:lint".into(),
            phone: None,
            display_name: Some("预检租户".into()),
            avatar_url: None,
            enterprise_profile: None,
            linux_uid: "claw-probe".into(),
            workspace_path: String::new(),
            port: None,
            paired_token_enc: None,
            status: "probe".into(),
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            last_error: None,
            pending_sop_name: None,
            breed: breed.to_string(),
        };
        let layout = UserHomeLayout::new(&self.cfg.zeroclaw.home_base, &user.linux_uid);
        self.build_ctx(&user, 40000, &layout, "probe-paired-token")
    }

    fn build_ctx(
        &self,
        user: &users::User,
        port: u16,
        layout: &UserHomeLayout,
        paired_token: &str,
    ) -> serde_json::Value {
        let profile: serde_json::Value = user
            .enterprise_profile
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));

        let tpl = &self.cfg.zeroclaw_template;
        json!({
            "paired_token": paired_token,
            "openid": user.openid,
            "phone": user.phone,
            "display_name": user.display_name,
            "avatar_url": user.avatar_url,
            "linux_uid": user.linux_uid,
            "port": port,
            // Base URL a tenant advertises for the files it produces. The
            // daemon signs download links against whatever this is; the HMAC
            // covers only path+expiry, so pointing it at the gateway is safe.
            // Falls back to loopback when no public base is configured, which
            // is exactly the previous behaviour.
            "download_base_url": if self.cfg.server.public_base_url.is_empty() {
                format!("http://127.0.0.1:{port}")
            } else {
                format!(
                    "{}/dl/{}",
                    self.cfg.server.public_base_url.trim_end_matches('/'),
                    user.linux_uid
                )
            },
            "breed": user.breed,
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
                "tavily_api_key": tpl.tavily_api_key,
            },
            "commodity": {
                "api_base": self.cfg.commodity.api_base,
                "detail_path_template": self.cfg.commodity.detail_path_template,
                "enabled": !self.cfg.commodity.api_base.is_empty(),
            },
            "activity": {
                "api_base": self.cfg.activity.api_base,
                "detail_path_template": self.cfg.activity.detail_path_template,
                "enabled": !self.cfg.activity.api_base.is_empty(),
            },
            "policy": {
                "api_base": self.cfg.policy.api_base,
                "detail_path_template": self.cfg.policy.detail_path_template,
                "enabled": !self.cfg.policy.api_base.is_empty(),
            },
            "policy_match": {
                "api_base": self.cfg.policy_match.api_base,
                "enterprise_profile_path": self.cfg.policy_match.enterprise_profile_path,
                "policy_list_path": self.cfg.policy_match.policy_list_path,
                "save_match_result_path": self.cfg.policy_match.save_match_result_path,
                "mini_program_detail_path_template": self.cfg.policy_match.mini_program_detail_path_template,
                "enabled": !self.cfg.policy_match.api_base.is_empty(),
            },
            "space": {
                "api_base": self.cfg.space.api_base,
                "park_detail_path_template": self.cfg.space.park_detail_path_template,
                "maker_space_detail_path_template": self.cfg.space.maker_space_detail_path_template,
            },
            "general_information": {
                "enabled": self.cfg.general_information.enabled,
            },
            "lead": {
                "webhook_url": self.cfg.lead.webhook_url,
                "webhook_format": self.cfg.lead.webhook_format,
                "enabled": !self.cfg.lead.webhook_url.is_empty(),
            },
            "http_allowed_domains_toml": http_allowed_domains_toml(&self.cfg),
            "qualification": {
                "api_base": self.cfg.qualification.api_base,
                "enterprise_profile_path": self.cfg.qualification.enterprise_profile_path,
                "qualification_check_path": self.cfg.qualification.qualification_check_path,
                "save_result_path": self.cfg.qualification.save_result_path,
                "mini_program_detail_path": self.cfg.qualification.mini_program_detail_path,
                "enabled": !self.cfg.qualification.api_base.is_empty(),
            },
            "sop_webhook": {
                "owner_openid": user.openid,
                "event_webhook_url": format!("http://127.0.0.1:{}/internal/sop-event", self.cfg.server.port),
                "qualification_reminder_url": format!("http://127.0.0.1:{}/internal/qualification-reminders", self.cfg.server.port),
            },
        })
    }

    async fn render_templates(
        &self,
        user: &users::User,
        port: u16,
        layout: &UserHomeLayout,
    ) -> Result<()> {
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

        let tpl_dir = self.breed_dir(&user.breed)?;
        let ctx = self.build_ctx(user, port, layout, &paired_token);
        render_workspace(&tpl_dir, layout, &ctx)
    }

    /// Liveness plus identity. `/health` only proves *a* daemon is up on that
    /// port — a foreign one answers just as happily, which is how a port
    /// collision once passed provisioning and surfaced hours later as a 401 on
    /// the first chat. `/api/status` requires the tenant's own bearer token, so
    /// it is the cheapest proof that the daemon on this port is really ours.
    async fn wait_ready(&self, openid: &str, port: u16) -> Result<()> {
        self.wait_health(port).await?;
        let user = users::get_required(&self.pool, openid).await?;
        match user.paired_token_enc {
            Some(token) => self.verify_port_identity(port, &token).await,
            // Nothing to prove identity with; liveness is all we have.
            None => Ok(()),
        }
    }

    /// Only 401/403 is treated as proof of a foreign daemon. Any other outcome
    /// (an older runtime without the endpoint, a transient error) is
    /// inconclusive and must not block provisioning — the check exists to catch
    /// collisions, not to add a new way for onboarding to fail.
    async fn verify_port_identity(&self, port: u16, paired_token: &str) -> Result<()> {
        let url = format!("http://127.0.0.1:{port}/api/status");
        let status = self
            .http
            .get(&url)
            .header(header::AUTHORIZATION, format!("Bearer {paired_token}"))
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0);
        match status {
            200 => Ok(()),
            401 | 403 => Err(Error::ZeroclawIdentityMismatch { port, status }),
            other => {
                tracing::warn!(
                    port,
                    status = other,
                    "identity probe inconclusive; accepting liveness alone"
                );
                Ok(())
            }
        }
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

    /// Refresh the rendered `USER.md` for an existing user from the latest
    /// profile in the DB. The other workspace files (IDENTITY/SOUL/config)
    /// are *not* touched — config.toml in particular contains the
    /// paired_token that the daemon already loaded.
    ///
    /// Re-render every markdown asset under the user's workspace
    /// (USER.md, IDENTITY.md, SOUL.md, all skills/<name>/SKILL.md)
    /// from the latest templates. **Does NOT touch config.toml** so
    /// the daemon's paired_token, port, and cost limits are preserved
    /// — caller doesn't need to restart the daemon, zeroclaw re-reads
    /// these markdown files on every new message
    /// (channels/mod.rs `inject_workspace_file`).
    ///
    /// Use after deploying template changes (e.g. new IDENTITY rules,
    /// new skills) to roll the change out to existing users without
    /// disturbing their session tokens.
    pub async fn refresh_workspace(&self, openid: &str) -> Result<()> {
        let user = users::get_required(&self.pool, openid).await?;
        let layout = UserHomeLayout::new(&self.cfg.zeroclaw.home_base, &user.linux_uid);
        let tpl_dir = self.breed_dir(&user.breed)?;

        // Reuse the existing paired_token from DB instead of generating
        // a new one — that keeps the daemon's already-loaded token valid
        // when we restart it below.
        let paired_token = user.paired_token_enc.clone().unwrap_or_else(|| {
            format!(
                "zc_{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )
        });

        let port_for_config = user.port.unwrap_or(0) as u16;
        let ctx = self.build_ctx(&user, port_for_config, &layout, &paired_token);
        render_workspace(&tpl_dir, &layout, &ctx)?;

        // chown best-effort (only matters in systemd backend)
        self.backend
            .chown_workspace(&user.linux_uid, &layout)
            .await
            .ok();

        // Restart the daemon so it re-reads config.toml. paired_token is
        // unchanged so existing client sessions stay valid; chat in
        // flight will fail once and the client should retry.
        if self.backend.launches_daemon() {
            if let Some(p) = user.port {
                self.backend.stop(&user.linux_uid).await.ok();
                self.backend.start(&user.linux_uid).await?;
                self.wait_ready(&user.openid, p as u16).await?;
            }
        }
        Ok(())
    }

    /// Re-render every tenant on one breed. This is what a template push
    /// from the development side triggers: a bundle for breed A must not
    /// restart the daemons of breed B's tenants, which is the whole
    /// difference between a swarm and a set of single-tenant servers.
    ///
    /// Errors are collected rather than propagated — one tenant whose
    /// daemon refuses to come back up must not stop the rollout to the
    /// rest. Returns `(refreshed, failures)`.
    pub async fn refresh_breed(&self, breed: &str) -> Result<(usize, Vec<(String, String)>)> {
        // Resolve once up front so a typo'd breed is an error, not an
        // empty-and-therefore-"successful" rollout.
        self.breed_dir(breed)?;
        let openids = users::openids_by_breed(&self.pool, breed).await?;
        let mut ok = 0usize;
        let mut failures = Vec::new();
        for openid in openids {
            match self.refresh_workspace(&openid).await {
                Ok(()) => ok += 1,
                Err(e) => failures.push((openid, e.to_string())),
            }
        }
        Ok((ok, failures))
    }


    /// Legacy: re-render only USER.md. Kept for /me/profile which
    /// only updates the user's own profile fields.
    pub async fn refresh_user_md(&self, openid: &str) -> Result<()> {
        let user = users::get_required(&self.pool, openid).await?;
        let layout = UserHomeLayout::new(&self.cfg.zeroclaw.home_base, &user.linux_uid);

        let profile: serde_json::Value = user
            .enterprise_profile
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| json!({}));

        let ctx = json!({
            "openid": user.openid,
            "phone": user.phone,
            "display_name": user.display_name,
            "avatar_url": user.avatar_url,
            "linux_uid": user.linux_uid,
            "port": user.port,
            "workspace_path": layout.workspace_dir,
            "config_dir": layout.config_dir,
            "home_dir": layout.home_dir,
            "enterprise": profile,
        });

        let mut hb = Handlebars::new();
        hb.set_strict_mode(false);

        let tpl_dir = self.breed_dir(&user.breed)?;
        std::fs::create_dir_all(&layout.workspace_dir)?;
        render_one(
            &hb,
            &tpl_dir,
            "USER.md.hbs",
            &layout.workspace_dir.join("USER.md"),
            &ctx,
        )?;

        // Re-chown so the new file is owned by the user (only matters in
        // systemd backend; mock no-ops). Best-effort.
        let user_md = layout.workspace_dir.join("USER.md");
        if user_md.exists() {
            self.backend
                .chown_workspace(&user.linux_uid, &layout)
                .await
                .ok();
        }
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
            self.wait_ready(&user.openid, port).await?;
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

/// Render one tenant's whole workspace from `tpl_dir`.
///
/// Shared by provision and refresh so the two can't disagree about which
/// files a breed consists of. Stale skills/SOPs/scripts are cleared
/// first: a skill dropped from the breed has to disappear from live
/// workspaces, otherwise the daemon keeps loading a capability the
/// operator believes they removed.
fn render_workspace(
    tpl_dir: &std::path::Path,
    layout: &UserHomeLayout,
    ctx: &serde_json::Value,
) -> Result<()> {
    let mut hb = Handlebars::new();
    hb.set_strict_mode(false);

    std::fs::create_dir_all(&layout.workspace_dir)?;
    std::fs::create_dir_all(&layout.config_dir)?;

    // AGENTS.md / MEMORY.md / HEARTBEAT.md / TOOLS.md are optional, but a
    // breed that ships them must get them rendered: zeroclaw feeds all four
    // into the system prompt (`agent/prompt.rs`), and beyond that AGENTS.md
    // drives `security/policy.rs` while HEARTBEAT.md drives
    // `heartbeat/engine.rs`. Dropping them turns a lobster that was tuned in
    // the workbench into a quietly different one on the swarm — no error,
    // just different behaviour, which is the worst failure mode for a
    // "what I tested is what runs" pipeline. `render_one` skips templates
    // that aren't there, so breeds that don't use them are unaffected.
    for fname in &[
        "USER.md",
        "IDENTITY.md",
        "SOUL.md",
        "AGENTS.md",
        "MEMORY.md",
        "HEARTBEAT.md",
        "TOOLS.md",
    ] {
        let out = layout.workspace_dir.join(fname);
        let tpl = tpl_dir.join(format!("{fname}.hbs"));
        if tpl.exists() {
            render_one(&hb, tpl_dir, &format!("{fname}.hbs"), &out, ctx)?;
        } else if out.exists() {
            // The breed no longer defines this file, so it must not stay in
            // the workspace. `render_one` alone only skips, which would let a
            // tenant moved off a breed keep running the old breed's
            // AGENTS.md security policy and HEARTBEAT.md schedule forever —
            // the same "capability the operator believes they removed" trap
            // that render_tree avoids by clearing skills/ and sops/ first.
            std::fs::remove_file(&out)?;
        }
    }
    render_one(
        &hb,
        tpl_dir,
        "config.toml.hbs",
        &layout.config_dir.join("config.toml"),
        ctx,
    )?;

    // skills: <breed>/skills/<name>/SKILL.md.hbs → <workspace>/skills/<name>/SKILL.md
    render_tree(&hb, tpl_dir, layout, ctx, "skills", &["SKILL.md"])?;
    // sops: <breed>/sops/<name>/{SOP.toml.hbs,SOP.md.hbs} → <workspace>/sops/<name>/…
    render_tree(&hb, tpl_dir, layout, ctx, "sops", &["SOP.toml", "SOP.md"])?;

    copy_scripts(tpl_dir, &layout.workspace_dir)?;
    Ok(())
}

/// Render `<tpl_dir>/<subdir>/<name>/<file>.hbs` for each `file` in
/// `files`, for every `<name>` directory present. Missing files are
/// skipped, so a SOP with no `SOP.toml` is fine.
fn render_tree(
    hb: &Handlebars,
    tpl_dir: &std::path::Path,
    layout: &UserHomeLayout,
    ctx: &serde_json::Value,
    subdir: &str,
    files: &[&str],
) -> Result<()> {
    let dest_root = layout.workspace_dir.join(subdir);
    let _ = std::fs::remove_dir_all(&dest_root);
    let src_root = tpl_dir.join(subdir);
    if !src_root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&src_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dest_dir = dest_root.join(entry.file_name());
        let mut wrote_any = false;
        for fname in files {
            let src = entry.path().join(format!("{fname}.hbs"));
            if !src.exists() {
                continue;
            }
            std::fs::create_dir_all(&dest_dir)?;
            let tpl_text = std::fs::read_to_string(&src)?;
            let rendered = hb.render_template(&tpl_text, ctx)?;
            std::fs::write(dest_dir.join(fname), rendered)?;
            wrote_any = true;
        }
        if !wrote_any {
            tracing::warn!(
                "{}/{} has none of {:?}; skipped",
                subdir,
                entry.file_name().to_string_lossy(),
                files
            );
        }
    }
    Ok(())
}

/// Copy `<breed>/scripts/` verbatim into `<workspace>/scripts/`.
///
/// Unlike skills and SOPs these are **not** handlebars-rendered: the tree
/// holds binary assets and source full of braces, both of which
/// templating would corrupt. Anything a script needs at runtime comes
/// from the environment (see `EnvironmentFile` in
/// `systemd/zeroclaw@.service`), not from the template context.
fn copy_scripts(tpl_dir: &std::path::Path, workspace_dir: &std::path::Path) -> Result<()> {
    let src = tpl_dir.join("scripts");
    let dest = workspace_dir.join("scripts");
    let _ = std::fs::remove_dir_all(&dest);
    if !src.is_dir() {
        return Ok(());
    }
    copy_dir_recursive(&src, &dest)
}

fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::UserHomeLayout;

    fn write(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    /// A breed carrying the four optional workspace files must get all of
    /// them rendered. They are not decoration: zeroclaw puts all four in the
    /// system prompt, and AGENTS.md additionally feeds `security/policy.rs`
    /// while HEARTBEAT.md feeds `heartbeat/engine.rs`. Before breeds existed
    /// only three .md files were ever written, so a lobster tuned in the
    /// OpenCode workbench would arrive on the swarm quietly missing its
    /// security policy and its schedule.
    #[test]
    fn renders_the_optional_workspace_files_a_breed_ships() {
        let tmp = std::env::temp_dir().join(format!("clawops-breed-{}", uuid::Uuid::new_v4().simple()));
        let tpl = tmp.join("tpl");
        for f in [
            "USER.md", "IDENTITY.md", "SOUL.md",
            "AGENTS.md", "MEMORY.md", "HEARTBEAT.md", "TOOLS.md",
        ] {
            write(&tpl.join(format!("{f}.hbs")), &format!("# {f} for {{{{display_name}}}}\n"));
        }
        write(&tpl.join("config.toml.hbs"), "port = {{port}}\n");

        let layout = UserHomeLayout::new(&tmp.join("home"), "claw-001");
        let ctx = serde_json::json!({"display_name": "测试租户", "port": 42001});
        render_workspace(&tpl, &layout, &ctx).expect("render");

        for f in [
            "USER.md", "IDENTITY.md", "SOUL.md",
            "AGENTS.md", "MEMORY.md", "HEARTBEAT.md", "TOOLS.md",
        ] {
            let got = std::fs::read_to_string(layout.workspace_dir.join(f))
                .unwrap_or_else(|e| panic!("{f} missing from workspace: {e}"));
            assert!(got.contains("测试租户"), "{f} was not rendered: {got:?}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Moving a tenant to a breed that does not define one of these files
    /// must remove it. Leaving it behind would keep the previous breed's
    /// AGENTS.md policy and HEARTBEAT.md schedule live on a tenant the
    /// operator believes they moved off it.
    #[test]
    fn drops_a_workspace_file_the_new_breed_no_longer_defines() {
        let tmp = std::env::temp_dir().join(format!("clawops-breed-{}", uuid::Uuid::new_v4().simple()));
        let rich = tmp.join("rich");
        let plain = tmp.join("plain");
        for d in [&rich, &plain] {
            write(&d.join("SOUL.md.hbs"), "soul {{display_name}}\n");
            write(&d.join("config.toml.hbs"), "port = {{port}}\n");
        }
        write(&rich.join("HEARTBEAT.md.hbs"), "every day {{display_name}}\n");
        write(&rich.join("AGENTS.md.hbs"), "policy {{display_name}}\n");

        let layout = UserHomeLayout::new(&tmp.join("home"), "claw-002");
        let ctx = serde_json::json!({"display_name": "甲", "port": 42002});

        render_workspace(&rich, &layout, &ctx).expect("render rich");
        assert!(layout.workspace_dir.join("HEARTBEAT.md").exists());
        assert!(layout.workspace_dir.join("AGENTS.md").exists());

        render_workspace(&plain, &layout, &ctx).expect("render plain");
        assert!(
            !layout.workspace_dir.join("HEARTBEAT.md").exists(),
            "stale HEARTBEAT.md survived the breed switch"
        );
        assert!(
            !layout.workspace_dir.join("AGENTS.md").exists(),
            "stale AGENTS.md survived the breed switch"
        );
        assert!(layout.workspace_dir.join("SOUL.md").exists(), "SOUL.md must stay");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
