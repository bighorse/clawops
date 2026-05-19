use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub commodity: CommodityConfig,
    #[serde(default)]
    pub activity: ActivityConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub policy_match: PolicyMatchConfig,
    #[serde(default)]
    pub space: SpaceConfig,
    #[serde(default)]
    pub general_information: GeneralInformationConfig,
    #[serde(default)]
    pub lead: LeadConfig,
    /// Per-SOP metadata for the task list display.
    /// Key is the sop_name (e.g. "policy-match").
    #[serde(default)]
    pub sop_metadata: HashMap<String, SopMetadata>,
    #[serde(default)]
    pub qualification: QualificationConfig,
}

/// Qualification-check SOP backend settings.
///
/// `api_base` is the same ztagent-service-api used by policy_match.
/// All AI analysis (existing qualification status + new recommendations)
/// is performed directly by the local zeroclaw LLM — no external AI call.
/// `sms_*` configure outbound SMS for mini-program users.
/// `wecom_webhook_url` is a Feishu/Lark bot webhook for wecom uin: users.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QualificationConfig {
    /// ztagent-service-api base, e.g. "https://bdhrapi.2048office.com/wecom"
    #[serde(default)]
    pub api_base: String,
    /// Path for enterprise profile sync (shared with policy-match)
    #[serde(default = "default_qual_enterprise_profile_path")]
    pub enterprise_profile_path: String,
    /// Path to sync qualification data from 天眼查 via backend
    #[serde(default = "default_qual_check_path")]
    pub qualification_check_path: String,
    /// Path to save qualification analysis results
    #[serde(default = "default_qual_save_result_path")]
    pub save_result_path: String,
    /// Mini-program qualification detail page path.
    /// {qualification_enterprise_id} is replaced at runtime.
    #[serde(default = "default_qual_mp_detail_path")]
    pub mini_program_detail_path: String,
    /// Generic SMS send endpoint. POST {"to": "<phone>", "content": "<text>"}
    #[serde(default)]
    pub sms_send_url: String,
    /// Bearer token for SMS endpoint
    #[serde(default)]
    pub sms_api_key: String,
    /// Feishu/Lark bot webhook for wecom uin: users
    #[serde(default)]
    pub wecom_webhook_url: String,
}

fn default_qual_enterprise_profile_path() -> String {
    "/agent/enterprise_profile_sync".into()
}
fn default_qual_check_path() -> String {
    "/agent/qualification_check".into()
}
fn default_qual_save_result_path() -> String {
    "/agent/save_qualification_result".into()
}
fn default_qual_mp_detail_path() -> String {
    "pages/qualification/index.html?id={qualification_enterprise_id}".into()
}

/// Metadata for one SOP. Drives:
/// - LLM classifier prompt (intent_description is included verbatim)
/// - Frontend display (display_name_cn + estimated_seconds)
/// - Cache TTL for /me/sop/tasks lookup
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SopMetadata {
    pub display_name_cn: String,
    pub estimated_seconds: u32,
    pub intent_description: String,
    #[serde(default = "default_sop_cache_ttl_days")]
    pub cache_ttl_days: u32,
}

fn default_sop_cache_ttl_days() -> u32 {
    7
}

/// Lead-submission notification settings. When a user leaves contact
/// info during cross-sell or commodity inquiry, the daemon can POST a
/// notification to a chat-bot webhook so ops sees it in real time.
/// If `webhook_url` is empty, no notification is sent.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LeadConfig {
    /// Webhook URL. Currently supports Lark/Feishu custom bot
    /// (`https://open.feishu.cn/open-apis/bot/v2/hook/<id>`).
    /// Empty disables notification — LLM only stores to memory.
    #[serde(default)]
    pub webhook_url: String,
    /// Webhook format. `"feishu"` (default) wraps in Lark text body;
    /// `"raw"` sends a flat JSON string.
    #[serde(default = "default_lead_format")]
    pub webhook_format: String,
}

fn default_lead_format() -> String {
    "feishu".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimitConfig {
    /// `/auth/wx-login` per source-IP per minute. Defaults: 10.
    pub wx_login_per_ip_per_min: u32,
    /// `/chat` per session-openid per minute. Defaults: 30.
    pub chat_per_user_per_min: u32,
    /// `/admin/*` per source-IP per minute. Defaults: 60.
    pub admin_per_ip_per_min: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            wx_login_per_ip_per_min: 10,
            chat_per_user_per_min: 30,
            admin_per_ip_per_min: 60,
        }
    }
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
    /// Tavily API key for the web_search tool. Empty = web_search stays
    /// disabled. Get a free key at https://app.tavily.com (1000/mo free).
    #[serde(default)]
    pub tavily_api_key: String,
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

/// Platform service-product (commodity) catalogue API. ClawOps doesn't
/// proxy these calls itself — it injects the API base + URL templates
/// into each user's SKILL doc so the in-daemon LLM uses the http_request
/// tool to query the catalogue and recommend products to the user.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommodityConfig {
    /// Public root, e.g. "https://bdhrapi.2048office.com/commodity".
    /// Empty disables the commodity skill.
    #[serde(default)]
    pub api_base: String,
    /// Mini-program internal path the LLM puts in markdown links so the
    /// front-end can intercept clicks and `wx.navigateTo`. `{id}` is
    /// replaced with the product id at render time by the LLM.
    /// Example: "/pages/commodity/detail?id={id}"
    #[serde(default = "default_detail_path_template")]
    pub detail_path_template: String,
}

fn default_detail_path_template() -> String {
    "/pages/products/detail?id={id}".into()
}

impl Default for CommodityConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            detail_path_template: default_detail_path_template(),
        }
    }
}

/// Activity catalogue API for Huairou Science City community events
/// (forums / roadshows / matchmaking sessions / training). Same
/// pattern as `CommodityConfig` — provisioner injects api_base +
/// detail_path_template into each user's SKILL.md so the daemon LLM
/// uses http_request to query the catalogue. Empty api_base
/// disables the activity skill (treated like commodity).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActivityConfig {
    #[serde(default)]
    pub api_base: String,
    #[serde(default = "default_activity_detail_path_template")]
    pub detail_path_template: String,
}

fn default_activity_detail_path_template() -> String {
    "/pages/activity/details/Index?obj_id={id}".into()
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            detail_path_template: default_activity_detail_path_template(),
        }
    }
}

/// Policy catalogue API. Same pattern as `CommodityConfig`. The skill
/// also exposes ancillary resources (categories tree, policy
/// declarations, announcements, FAQs) to help the LLM ground its
/// answers in real data instead of stale handbook numbers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub api_base: String,
    #[serde(default = "default_policy_detail_path_template")]
    pub detail_path_template: String,
}

fn default_policy_detail_path_template() -> String {
    "/pages/policy/detail?id={id}".into()
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            detail_path_template: default_policy_detail_path_template(),
        }
    }
}

/// Policy-match SOP backend. Unlike `PolicyConfig` (which points at the
/// public 2048office policy catalogue for the query-style policy-recommend
/// skill), this points at the internal ztagent-service-api that owns the
/// enterprise profile + policy match writeback. The SOP calls three
/// endpoints in one run: profile-sync (GET), policy-list (GET), and
/// save-result (POST, also triggers the mini-program card push).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyMatchConfig {
    /// Base URL of ztagent-service-api, e.g. "https://bdhrapi.2048office.com".
    /// Empty disables policy-match — daemon http_request will fail.
    #[serde(default)]
    pub api_base: String,
    /// GET path returning {enterprise_id, qualification_enterprise_id,
    /// basic_info, qualification_info, bocha_info}. NEW endpoint to be
    /// added in ztagent-service-api (synchronous, no rabbitmq).
    #[serde(default = "default_pm_enterprise_profile_path")]
    pub enterprise_profile_path: String,
    /// GET path for policy catalogue. Existing CRUD on `PolicySummary`.
    #[serde(default = "default_pm_policy_list_path")]
    pub policy_list_path: String,
    /// POST path to persist match result and trigger mini-program push.
    /// NEW endpoint to be added in ztagent-service-api (atomic delete+insert
    /// of EnterprisePolicySummary + EnterprisePolicySummaryCondition rows).
    #[serde(default = "default_pm_save_match_result_path")]
    pub save_match_result_path: String,
    /// Mini-program detail path the LLM puts in card-style links so the
    /// front-end can intercept clicks and wx.navigateTo.
    /// `{qualification_enterprise_id}` is replaced at render time by the LLM
    /// with the value from step 1's profile.json (NOT `enterprise_id` —
    /// the mini-program routes by the qualification table PK).
    #[serde(default = "default_pm_detail_path_template")]
    pub mini_program_detail_path_template: String,
}

fn default_pm_enterprise_profile_path() -> String {
    "/agent/enterprise_profile_sync".into()
}
fn default_pm_policy_list_path() -> String {
    "/policy_summary".into()
}
fn default_pm_save_match_result_path() -> String {
    "/agent/save_match_result".into()
}
fn default_pm_detail_path_template() -> String {
    "/pages/recommendation/index?id={qualification_enterprise_id}".into()
}

impl Default for PolicyMatchConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            enterprise_profile_path: default_pm_enterprise_profile_path(),
            policy_list_path: default_pm_policy_list_path(),
            save_match_result_path: default_pm_save_match_result_path(),
            mini_program_detail_path_template: default_pm_detail_path_template(),
        }
    }
}

/// Space (parks / maker-spaces / workshops / offices / stations /
/// industrial land) catalogue API. Same provisioner-injection pattern
/// as commodity/activity/policy, but **no enable toggle** — the
/// space-recommend skill is always rendered. Defaults pre-populate
/// production values so the [space] table can be omitted entirely
/// from clawops.toml.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpaceConfig {
    #[serde(default = "default_space_api_base")]
    pub api_base: String,
    #[serde(default = "default_space_park_detail_path_template")]
    pub park_detail_path_template: String,
    #[serde(default = "default_space_maker_space_detail_path_template")]
    pub maker_space_detail_path_template: String,
}

fn default_space_api_base() -> String {
    "https://bdhrapi.2048office.com/space-v2".into()
}

fn default_space_park_detail_path_template() -> String {
    "/pages/space/park?id={id}".into()
}

fn default_space_maker_space_detail_path_template() -> String {
    "/pages/space/makerDetail?id={id}".into()
}

impl Default for SpaceConfig {
    fn default() -> Self {
        Self {
            api_base: default_space_api_base(),
            park_detail_path_template: default_space_park_detail_path_template(),
            maker_space_detail_path_template: default_space_maker_space_detail_path_template(),
        }
    }
}

/// Static-knowledge skill carrying the Huairou Science City service
/// handbook (facilities, policies, onboarding flow, contacts) inlined
/// into each user's prompt. No external API — flipping `enabled` to
/// false simply skips rendering the SKILL.md, so the LLM no longer
/// holds that knowledge and won't try to answer those questions.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeneralInformationConfig {
    #[serde(default = "default_general_information_enabled")]
    pub enabled: bool,
}

fn default_general_information_enabled() -> bool {
    true
}

impl Default for GeneralInformationConfig {
    fn default() -> Self {
        Self {
            enabled: default_general_information_enabled(),
        }
    }
}

/// WeChat code-to-openid exchange settings.
///
/// ClawOps does **not** call WeChat's `jscode2session` directly — that
/// would consume the same `access_token` the upstream platform backend
/// already uses, and the wx `code` is single-use. Instead, ClawOps POSTs
/// the code to the platform's exchange endpoint and gets back the openid.
///
/// Endpoint shape:
///   `POST {backend_base_url}/message/wechat/applets/{app_id}/open_id`
///   body: `{"code": "...", "client": "clawops"}`
///   resp 200: `{"data": {"open_id": "..."}}`
///   non-200: `{"message": "..."}`  (403 = unconfigured app_id, 500 = code expired/used)
///
/// Mock mode: leave `backend_base_url` empty for local dev — the request
/// body's `mock_openid` is trusted directly. Production deployments MUST
/// set `backend_base_url`; doing so also makes `mock_openid` rejected.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WxConfig {
    #[serde(default)]
    pub backend_base_url: String,
    #[serde(default = "default_wx_exchange_timeout_secs")]
    pub exchange_timeout_secs: u64,
}

fn default_wx_exchange_timeout_secs() -> u64 {
    30
}

impl Default for WxConfig {
    fn default() -> Self {
        Self {
            backend_base_url: String::new(),
            exchange_timeout_secs: default_wx_exchange_timeout_secs(),
        }
    }
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
