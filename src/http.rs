use crate::auth::WxClient;
use crate::config::Config;
use crate::limits::{self, AppLimiters};
use crate::provisioner::Provisioner;
use crate::qualification_reminders;
use crate::sop_tasks;
use crate::{chat_history, sessions, users, Error, Result};
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::extract::{Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub cfg: Arc<Config>,
    pub provisioner: Arc<Provisioner>,
    pub http: reqwest::Client,
    pub wx: Arc<WxClient>,
    pub limiters: Arc<AppLimiters>,
    /// Broadcast channel for SOP task events. Subscribers (each /events
    /// SSE connection) filter by openid (stripped before forwarding).
    /// Capacity 256 is plenty given event frequency (a few/sec at peak).
    pub sop_event_tx: broadcast::Sender<JsonValue>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/auth/wx-login", post(wx_login))
        .route("/auth/wecom-login", post(wecom_login))
        .route("/auth/logout", post(logout))
        .route("/auth/logout-all", post(logout_all))
        .route("/chat", post(chat))
        .route("/events", get(events))
        .route("/me/profile", axum::routing::put(update_my_profile))
        .route("/me/profile", get(get_my_profile))
        .route("/me/chat-history", get(get_my_chat_history))
        .route("/me/sop/tasks", get(list_my_sop_tasks))
        .route("/me/qualification-reminders", get(list_my_qualification_reminders))
        .route("/internal/qualification-reminders", post(internal_qualification_reminders))
        .route("/admin/users", get(list_users))
        .route("/admin/users/:openid", get(get_user))
        .route("/admin/provision", post(admin_provision))
        .route("/admin/stop/:openid", post(admin_stop))
        .route("/admin/issue-token", post(admin_issue_token))
        .route("/admin/refresh-workspace/:openid", post(admin_refresh_workspace))
        .route("/admin/refresh-all-workspaces", post(admin_refresh_all_workspaces))
        .route("/internal/sop-event", post(internal_sop_event))
        .with_state(state)
}

/// Bearer-token extractor — resolves the user's `openid` from the
/// `Authorization: Bearer <session>` header. Returns 401 if missing or invalid.
pub struct AuthOpenid(pub String);

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthOpenid {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let auth = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| {
                (StatusCode::UNAUTHORIZED, "missing bearer token").into_response()
            })?;
        sessions::resolve(&state.pool, auth)
            .await
            .map(AuthOpenid)
            .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()).into_response())
    }
}

/// Admin guard — rate-limit by source IP, then `X-Admin-Token` constant-time
/// compare. 503 if admin is disabled (token empty), 429 if rate-limited.
pub struct AdminGuard;

#[axum::async_trait]
impl FromRequestParts<AppState> for AdminGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        // 1. rate limit on source IP first (cheap; stops scanners early)
        let ip = limits::client_ip(&parts.headers);
        if let Err(retry) = limits::check(&state.limiters.admin_per_ip, &ip) {
            return Err(Error::RateLimited {
                retry_after_secs: retry,
            }
            .into_response());
        }

        let expected = state.cfg.admin.token.as_bytes();
        if expected.is_empty() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "admin api is disabled (admin.token empty in clawops.toml)",
            )
                .into_response());
        }
        let supplied = parts
            .headers
            .get("X-Admin-Token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .as_bytes();
        if !ct_eq(supplied, expected) {
            return Err((StatusCode::UNAUTHORIZED, "invalid admin token").into_response());
        }
        Ok(AdminGuard)
    }
}

/// Constant-time comparison; returns false on length mismatch without
/// short-circuiting on first differing byte (only on length).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// GET /events — Server-Sent Events stream merging:
///   1. The user's zeroclaw daemon /api/events (raw byte pass-through),
///   2. ClawOps' own `sop_task` events broadcast from the chat handler
///      and sop_runner.
///
/// Auth via `Authorization: Bearer <session_token>` (or `?token=` for
/// EventSource clients that can't set headers).
///
/// Per-user filtering: sop_task events carry an `openid` field in the
/// broadcast payload; this handler strips it before forwarding and
/// only forwards events matching the connected user's openid.
#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    token: Option<String>,
}

async fn events(
    State(st): State<AppState>,
    Query(q): Query<EventsQuery>,
    parts: axum::http::HeaderMap,
) -> std::result::Result<Response, Error> {
    let token = parts
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or(q.token)
        .ok_or_else(|| Error::Other("missing bearer token".into()))?;
    let openid = sessions::resolve(&st.pool, &token).await?;

    let user = st.provisioner.ensure_running(&openid).await?;
    users::touch_active(&st.pool, &user.openid).await?;

    // mpsc channel used to merge both streams into a single Body::from_stream
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<axum::body::Bytes, axum::Error>>(64);

    // Spawn task 1: forward daemon SSE byte stream (real backend only)
    if st.provisioner.backend.launches_daemon() {
        let port = user.port.ok_or_else(|| {
            Error::Other(format!("user {} has no port assigned", user.openid))
        })?;
        let paired_token = user
            .paired_token_enc
            .as_deref()
            .ok_or_else(|| Error::Other("paired token missing".into()))?
            .to_string();
        let http = st.http.clone();
        let tx_daemon = tx.clone();
        tokio::spawn(async move {
            let upstream = match http
                .get(format!("http://127.0.0.1:{port}/api/events"))
                .header(header::AUTHORIZATION, format!("Bearer {paired_token}"))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    tracing::warn!("upstream /api/events returned {}", r.status());
                    return;
                }
                Err(e) => {
                    tracing::warn!("upstream /api/events fetch error: {e}");
                    return;
                }
            };
            let mut stream = upstream.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(_) => break,
                };
                if tx_daemon.send(Ok(bytes)).await.is_err() {
                    break; // client disconnected
                }
            }
        });
    } else {
        // Mock backend: emit a one-shot synthetic event so the SSE
        // plumbing on the client side can still be exercised.
        let mock = format!(
            "data: {{\"type\":\"mock_hello\",\"openid\":\"{}\"}}\n\n",
            user.openid
        );
        let _ = tx.send(Ok(axum::body::Bytes::from(mock))).await;
    }

    // Spawn task 2: forward clawops sop_task broadcast (filtered by openid)
    let mut rx_bc = st.sop_event_tx.subscribe();
    let tx_clawops = tx.clone();
    let openid_filter = user.openid.clone();
    tokio::spawn(async move {
        loop {
            let value = match rx_bc.recv().await {
                Ok(v) => v,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break, // channel closed
            };
            let event_openid = match value.get("openid").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            if event_openid != openid_filter {
                continue;
            }
            // Strip openid before sending to client (not needed by FE)
            let mut clean = value.clone();
            if let Some(obj) = clean.as_object_mut() {
                obj.remove("openid");
            }
            let frame = format!("data: {}\n\n", clean);
            if tx_clawops.send(Ok(axum::body::Bytes::from(frame))).await.is_err() {
                break;
            }
        }
    });

    // Drop the original sender so the stream closes when both task
    // senders drop (client disconnect + broadcast end).
    drop(tx);

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .map_err(|e| Error::Other(format!("response build: {e}")))?)
}

#[derive(Serialize)]
struct HealthResp {
    status: &'static str,
    version: &'static str,
}

async fn health() -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Deserialize)]
struct WxLoginReq {
    /// Mini-program app_id. Required in production. Forwarded to the
    /// platform backend, which rejects (403) any app_id not configured
    /// on its side — so ClawOps does not maintain its own whitelist.
    #[serde(default)]
    app_id: String,
    /// Code returned by `wx.login()` on the mini-program side.
    #[serde(default)]
    code: String,
    /// Mock openid used when wx.backend_base_url is empty (dev only).
    #[serde(default)]
    mock_openid: Option<String>,
    /// Display name (from `<input type="nickname">` on the mini-program).
    /// WeChat no longer exposes a code-to-nickname API since 2022, so
    /// this can only come from the client.
    #[serde(default)]
    display_name: Option<String>,
    /// Avatar URL (from `<button open-type="chooseAvatar">`). Caller
    /// must already have uploaded the wxfile:// tempfile to a permanent
    /// store — ClawOps stores the URL verbatim.
    #[serde(default)]
    avatar_url: Option<String>,
    /// Optional enterprise profile JSON (rarely set on first login —
    /// the LLM normally fills it during chat). Kept for back-compat.
    #[serde(default)]
    enterprise_profile: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct WxLoginResp {
    token: String,
    openid: String,
    is_new_user: bool,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// POST /auth/wecom-login — provision a user identified by their
/// Enterprise WeChat `uin` (string ID assigned by the wecom platform
/// when a WeChat user adds our enterprise contact). This is a
/// server-to-server endpoint called by the platform's wecom bot
/// backend, not by an end-user device.
///
/// Identity model: the uin user is **completely independent** of any
/// mini-program (wx-login) user — they get their own daemon, workspace,
/// memory, and chat history. We synthesise an openid of the form
/// `uin:<uin>` to keep the existing users table primary key intact;
/// nothing downstream needs to change.
#[derive(Deserialize)]
struct WecomLoginReq {
    /// Enterprise WeChat-assigned uin (≤128 chars; caller validates format).
    uin: String,
    /// Optional display name fetched from wecom contact profile.
    #[serde(default)]
    display_name: Option<String>,
    /// Optional avatar URL fetched from wecom contact profile.
    #[serde(default)]
    avatar_url: Option<String>,
}

async fn wecom_login(
    _: AdminGuard,
    State(st): State<AppState>,
    Json(req): Json<WecomLoginReq>,
) -> std::result::Result<Json<WxLoginResp>, Error> {
    let openid = format!("uin:{}", req.uin);
    let mut is_new_user = false;
    if users::get(&st.pool, &openid).await?.is_none() {
        is_new_user = true;
        let new = users::NewUser {
            openid: openid.clone(),
            phone: None,
            display_name: req.display_name,
            avatar_url: req.avatar_url,
            enterprise_profile: None,
        };
        st.provisioner.provision(&new).await?;
    } else if req.display_name.is_some() || req.avatar_url.is_some() {
        let patch = users::ProfilePatch {
            display_name: req.display_name,
            phone: None,
            avatar_url: req.avatar_url,
            enterprise_profile: None,
        };
        users::update_profile(&st.pool, &openid, &patch).await?;
    } else {
        users::touch_active(&st.pool, &openid).await?;
    }

    let s = sessions::issue(&st.pool, &openid, None).await?;
    Ok(Json(WxLoginResp {
        token: s.token,
        openid,
        is_new_user,
        expires_at: s.expires_at,
    }))
}

async fn wx_login(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<WxLoginReq>,
) -> std::result::Result<Json<WxLoginResp>, Error> {
    // Rate-limit by source IP (defends openid enumeration / brute force)
    let ip = limits::client_ip(&headers);
    limits::check(&st.limiters.wx_login_per_ip, &ip)
        .map_err(|retry| Error::RateLimited { retry_after_secs: retry })?;

    let session = st
        .wx
        .code2session(&req.app_id, &req.code, req.mock_openid.as_deref())
        .await?;

    let openid = session.openid.clone();
    let mut is_new_user = false;
    if users::get(&st.pool, &openid).await?.is_none() {
        is_new_user = true;
        let new = users::NewUser {
            openid: openid.clone(),
            phone: None,
            display_name: req.display_name,
            avatar_url: req.avatar_url,
            enterprise_profile: req.enterprise_profile,
        };
        st.provisioner.provision(&new).await?;
    } else {
        // Returning user: opportunistically refresh display_name / avatar
        // if the client supplied newer values (e.g. user changed nickname
        // in mini-program profile page and re-logged in).
        if req.display_name.is_some() || req.avatar_url.is_some() {
            let patch = users::ProfilePatch {
                display_name: req.display_name,
                phone: None,
                avatar_url: req.avatar_url,
                enterprise_profile: None,
            };
            users::update_profile(&st.pool, &openid, &patch).await?;
        } else {
            users::touch_active(&st.pool, &openid).await?;
        }
    }

    let s = sessions::issue(&st.pool, &openid, None).await?;
    Ok(Json(WxLoginResp {
        token: s.token,
        openid,
        is_new_user,
        expires_at: s.expires_at,
    }))
}

#[derive(Deserialize)]
struct ChatReq {
    content: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Serialize)]
struct ChatResp {
    response: String,
    model: Option<String>,
    openid: String,
}

async fn chat(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
    Json(req): Json<ChatReq>,
) -> std::result::Result<Json<ChatResp>, Error> {
    limits::check(&st.limiters.chat_per_user, &openid)
        .map_err(|retry| Error::RateLimited { retry_after_secs: retry })?;

    let user = st.provisioner.ensure_running(&openid).await?;
    users::touch_active(&st.pool, &user.openid).await?;

    // If a previous turn asked the user for their company name (gateway-level
    // intercept), automatically re-trigger the SOP once a valid name arrives.
    // We synthesize "{company}，{trigger}" so zeroclaw calls sop_execute again
    // and from_message finds the company name in the same turn.
    let message_to_zeroclaw: String = {
        let db_user = users::get(&st.pool, &user.openid).await.ok().flatten();
        if let Some(pending) = db_user.and_then(|u| u.pending_sop_name.clone()) {
            if let Some(company) = extract_company_name_from_text(&req.content) {
                if let Err(e) = users::clear_pending_sop(&st.pool, &user.openid).await {
                    tracing::warn!(openid = %user.openid, "failed to clear pending_sop_name: {e}");
                }
                let trigger = sop_trigger_phrase(&pending);
                tracing::debug!(openid = %user.openid, company = %company, pending_sop = %pending,
                    "pending SOP: company name received, synthesizing re-trigger");
                format!("{company}，{trigger}")
            } else {
                let _ = users::clear_pending_sop(&st.pool, &user.openid).await;
                req.content.clone()
            }
        } else {
            req.content.clone()
        }
    };

    // sop_started / sop_name extracted from zeroclaw response when a SOP was triggered.
    let mut sop_started = false;
    let mut sop_name_from_zc: Option<String> = None;

    let (response_text, model) = if !st.provisioner.backend.launches_daemon() {
        (format!("[mock] echo: {}", req.content), Some("mock".into()))
    } else {
        let port = user
            .port
            .ok_or_else(|| Error::Other(format!("user {} has no port assigned", user.openid)))?;
        let url = format!("http://127.0.0.1:{port}/api/chat");

        let mut builder = st
            .http
            .post(&url)
            .timeout(std::time::Duration::from_secs(900))
            .json(&serde_json::json!({"message": message_to_zeroclaw}));
        if let Some(token) = &user.paired_token_enc {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(idem) = &req.idempotency_key {
            builder = builder.header("X-Idempotency-Key", idem);
        }

        let resp = builder.send().await?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "zeroclaw /api/chat returned {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp.json().await?;
        sop_started = body.get("sop_started").and_then(|v| v.as_bool()).unwrap_or(false);
        sop_name_from_zc = body.get("sop_name").and_then(|v| v.as_str()).map(String::from);
        let response_text = body
            .get("response")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        (response_text, model)
    };

    // ── SOP post-processing ──────────────────────────────────────
    // zeroclaw returned early because it triggered a SOP.
    // (1) Validate enterprise name — if missing/abbreviated, override the
    //     response with a prompt asking for the full legal name.
    // (2) Translate the internal sop_name ("policy-match") to Chinese.
    if sop_started {
        if let Some(ref sop_name) = sop_name_from_zc {
            let db_user = users::get(&st.pool, &user.openid).await.ok().flatten();

            // Two sources only. Memory (brain.db) is intentionally excluded: it holds the
            // PREVIOUS company and would cause the SOP to run for the wrong company when the
            // user switches ("换成拓尔思"). Only trust what's authoritative right now:
            // 1. DB enterprise_profile set via the profile-update API
            // 2. Full legal name explicitly typed in this message
            let from_db: Option<String> = db_user
                .as_ref()
                .and_then(|u| u.enterprise_profile.as_ref())
                .and_then(|s| serde_json::from_str::<JsonValue>(s).ok())
                .and_then(|v| v.get("company_name").and_then(|x| x.as_str()).map(String::from))
                .filter(|n| looks_like_full_company_name(n));

            let from_message: Option<String> = if from_db.is_none() {
                extract_company_name_from_text(&req.content)
            } else {
                None
            };

            // 3rd source: recent chat history — used whenever the current message
            // contains no company name. If from_message already extracted a name (user
            // specified a new company inline), from_history is skipped. If the user
            // says a bare SOP trigger ("政策匹配") without naming a company, we look in
            // history rather than prompting again — this matches user expectation and
            // avoids the "amnesia" loop where every re-trigger re-asks for the name.
            //
            // Exception: if the current message asserts a NEW company identity ("我是X",
            // "换成X"), skip from_history entirely — returning a stale old company name
            // would silently trigger the SOP for the wrong entity.
            let from_history: Option<String> = if from_db.is_none() && from_message.is_none() && !message_asserts_new_company(&req.content) {
                match chat_history::fetch_page(&st.pool, &user.openid, None, 10).await {
                    Ok(msgs) => msgs
                        .iter()
                        .filter(|m| m.role == "user")
                        .find_map(|m| extract_company_name_from_text(&m.content)),
                    Err(_) => None,
                }
            } else {
                None
            };

            let enterprise_name = from_db.or(from_message).or(from_history);

            if enterprise_name.is_none() {
                let prompt = "请提供您企业的完整全称（营业执照上的名称，通常以「有限公司」结尾），我将为您发起政策匹配评测。".to_string();
                tracing::debug!(openid = %user.openid, sop_name = %sop_name,
                    "SOP started but no verified enterprise_name — overriding response");
                if let Err(e) = chat_history::record_turn(&st.pool, &user.openid, &req.content, &prompt).await {
                    tracing::warn!(openid = %user.openid, "failed to persist chat turn: {e}");
                }
                if let Err(e) = users::set_pending_sop(&st.pool, &user.openid, sop_name).await {
                    tracing::warn!(openid = %user.openid, "failed to set pending_sop_name: {e}");
                }
                return Ok(Json(ChatResp {
                    response: prompt,
                    model: Some("clawops-router".to_string()),
                    openid: user.openid,
                }));
            }

            // Enterprise name valid — create the pending task here (not in the webhook,
            // to avoid orphan rows when validation blocks the request).
            let sop_meta = st.cfg.sop_metadata.get(sop_name.as_str());
            let estimated_seconds = sop_meta.map(|m| m.estimated_seconds).unwrap_or(540);
            let task_id = sop_tasks::new_task_id();
            if let Err(e) = sop_tasks::insert_pending(
                &st.pool,
                &task_id,
                &user.openid,
                sop_name,
                enterprise_name.as_deref(),
                estimated_seconds,
            )
            .await
            {
                tracing::warn!(openid = %user.openid, task_id = %task_id, "failed to insert pending sop task: {e}");
            } else {
                emit_sop_event(&st, &task_id, &user.openid, "created");
            }

            let display_name = sop_meta
                .map(|m| m.display_name_cn.as_str())
                .unwrap_or(sop_name.as_str());
            // WeChat/WeCom users (uin: prefix) have no task-list UI; give a
            // simple wait message. The SOP itself delivers the final reply via
            // the backend messaging channel. Mini-program users get the full
            // task-list prompt.
            let chat_response = if user.openid.starts_with("uin:") {
                format!("正在为您处理「{}」，请稍候，完成后会主动通知您。", display_name)
            } else {
                format!("已为您发起「{}」，可在右上角任务列表查看进度。", display_name)
            };
            if let Err(e) = chat_history::record_turn(&st.pool, &user.openid, &req.content, &chat_response).await {
                tracing::warn!(openid = %user.openid, "failed to persist chat turn: {e}");
            }
            return Ok(Json(ChatResp {
                response: chat_response,
                model,
                openid: user.openid,
            }));
        }
    }

    let response_text = sanitize_assistant_response(&response_text);

    if let Err(e) =
        chat_history::record_turn(&st.pool, &user.openid, &req.content, &response_text).await
    {
        tracing::warn!(openid = %user.openid, "failed to persist chat turn: {e}");
    }

    Ok(Json(ChatResp {
        response: response_text,
        model,
        openid: user.openid,
    }))
}

fn emit_sop_event(st: &AppState, task_id: &str, openid: &str, event: &str) {
    let _ = st.sop_event_tx.send(serde_json::json!({
        "type": "sop_task",
        "task_id": task_id,
        "event": event,
        "openid": openid,
    }));
}

#[derive(Deserialize)]
struct ChatHistoryQuery {
    /// Cursor: returns messages with `id < before_id`. Omit on first page.
    #[serde(default)]
    before_id: Option<i64>,
    /// Page size, capped at 100. Default 20.
    #[serde(default = "default_history_limit")]
    limit: i64,
}

fn default_history_limit() -> i64 {
    20
}

#[derive(Serialize)]
struct ChatHistoryResp {
    /// Messages in DESC order by id (newest first within this page).
    /// Front-end typically reverses for display.
    messages: Vec<chat_history::ChatMessage>,
    /// True if more messages exist before this page.
    has_more: bool,
    /// Pass this back as `before_id` to load the next older page.
    /// Null when `has_more` is false.
    next_cursor: Option<i64>,
}

async fn get_my_chat_history(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
    Query(q): Query<ChatHistoryQuery>,
) -> std::result::Result<Json<ChatHistoryResp>, Error> {
    let messages = chat_history::fetch_page(&st.pool, &openid, q.before_id, q.limit).await?;
    let has_more = messages.len() as i64 == q.limit.clamp(1, 100);
    let next_cursor = if has_more {
        messages.last().map(|m| m.id)
    } else {
        None
    };
    Ok(Json(ChatHistoryResp {
        messages,
        has_more,
        next_cursor,
    }))
}

/// POST /auth/logout — revoke the bearer used on this request.
/// Idempotent: missing/already-revoked token still returns 200.
async fn logout(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> std::result::Result<Json<serde_json::Value>, Error> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    // We DON'T require AuthOpenid here — the token may already be expired
    // and the client just wants to clean up. Revoking a non-existent token
    // is a 0-rows no-op (safe & idempotent).
    let revoked = sessions::revoke(&st.pool, token).await.unwrap_or(0);
    Ok(Json(serde_json::json!({ "revoked": revoked })))
}

/// POST /auth/logout-all — revoke every session for the authenticated
/// openid. Use after device loss or suspected token compromise.
async fn logout_all(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
) -> std::result::Result<Json<serde_json::Value>, Error> {
    let revoked = sessions::revoke_all_for_openid(&st.pool, &openid).await?;
    Ok(Json(serde_json::json!({
        "revoked": revoked,
        "openid": openid,
    })))
}

#[derive(Serialize)]
struct MyProfileResp {
    openid: String,
    display_name: Option<String>,
    avatar_url: Option<String>,
    phone: Option<String>,
    enterprise_profile: Option<serde_json::Value>,
}

async fn get_my_profile(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
) -> std::result::Result<Json<MyProfileResp>, Error> {
    let u = users::get_required(&st.pool, &openid).await?;
    let prof = u
        .enterprise_profile
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    Ok(Json(MyProfileResp {
        openid: u.openid,
        display_name: u.display_name,
        avatar_url: u.avatar_url,
        phone: u.phone,
        enterprise_profile: prof,
    }))
}

async fn update_my_profile(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
    Json(patch): Json<users::ProfilePatch>,
) -> std::result::Result<Json<MyProfileResp>, Error> {
    users::update_profile(&st.pool, &openid, &patch).await?;
    // Re-render USER.md so the next /chat picks up the new profile —
    // zeroclaw reads the file on every new message, no daemon restart.
    st.provisioner.refresh_user_md(&openid).await?;
    get_my_profile(State(st), AuthOpenid(openid)).await
}

async fn list_users(
    _: AdminGuard,
    State(st): State<AppState>,
) -> Result<Json<Vec<users::User>>> {
    let rows: Vec<users::User> = sqlx::query_as(
        "SELECT * FROM users ORDER BY created_at DESC",
    )
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

async fn get_user(
    _: AdminGuard,
    State(st): State<AppState>,
    Path(openid): Path<String>,
) -> std::result::Result<Json<users::User>, Error> {
    let u = users::get_required(&st.pool, &openid).await?;
    Ok(Json(u))
}

#[derive(Deserialize)]
struct ProvisionReq {
    openid: String,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    enterprise_profile: Option<serde_json::Value>,
}

async fn admin_provision(
    _: AdminGuard,
    State(st): State<AppState>,
    Json(req): Json<ProvisionReq>,
) -> std::result::Result<impl IntoResponse, Error> {
    let new = users::NewUser {
        openid: req.openid,
        phone: req.phone,
        display_name: req.display_name,
        avatar_url: req.avatar_url,
        enterprise_profile: req.enterprise_profile,
    };
    let out = st.provisioner.provision(&new).await?;
    Ok(Json(serde_json::json!({
        "openid": out.openid,
        "linux_uid": out.linux_uid,
        "port": out.port,
        "workspace": out.workspace_path,
        "paired": out.paired,
    })))
}

#[derive(Deserialize)]
struct IssueTokenReq {
    openid: String,
}

/// POST /admin/issue-token — sign a fresh 30-day session token for an
/// existing openid. Use case: ops / debugging from a Mac without going
/// through real wx.login(). Returns 404 if the openid hasn't been
/// provisioned yet (use /admin/provision first to create the user).
async fn admin_issue_token(
    _: AdminGuard,
    State(st): State<AppState>,
    Json(req): Json<IssueTokenReq>,
) -> std::result::Result<impl IntoResponse, Error> {
    // Confirm the user exists; refuse to issue tokens for unknown openids
    // (otherwise an admin typo creates a stranded session).
    users::get_required(&st.pool, &req.openid).await?;
    let s = sessions::issue(&st.pool, &req.openid, Some("admin-issued")).await?;
    Ok(Json(serde_json::json!({
        "token": s.token,
        "openid": s.openid,
        "expires_at": s.expires_at,
    })))
}

/// POST /admin/refresh-workspace/:openid — re-render workspace markdown
/// (USER.md, IDENTITY.md, SOUL.md, skills/*) from the latest templates
/// for ONE user. Does not touch config.toml so paired_token / cost
/// limits survive. zeroclaw picks up changes on next /chat (re-reads
/// markdown per turn).
async fn admin_refresh_workspace(
    _: AdminGuard,
    State(st): State<AppState>,
    Path(openid): Path<String>,
) -> std::result::Result<impl IntoResponse, Error> {
    st.provisioner.refresh_workspace(&openid).await?;
    Ok(Json(serde_json::json!({"refreshed": openid})))
}

/// POST /admin/refresh-all-workspaces — same as above but loop over every
/// user. Reports success/error counts. Use after deploying template changes.
async fn admin_refresh_all_workspaces(
    _: AdminGuard,
    State(st): State<AppState>,
) -> std::result::Result<impl IntoResponse, Error> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT openid FROM users")
        .fetch_all(&st.pool)
        .await?;
    let mut ok = 0usize;
    let mut errors: Vec<serde_json::Value> = Vec::new();
    for (openid,) in &rows {
        match st.provisioner.refresh_workspace(openid).await {
            Ok(()) => ok += 1,
            Err(e) => errors.push(serde_json::json!({
                "openid": openid,
                "error": e.to_string(),
            })),
        }
    }
    Ok(Json(serde_json::json!({
        "total": rows.len(),
        "refreshed": ok,
        "errors": errors,
    })))
}

async fn admin_stop(
    _: AdminGuard,
    State(st): State<AppState>,
    Path(openid): Path<String>,
) -> std::result::Result<impl IntoResponse, Error> {
    st.provisioner.stop(&openid).await?;
    Ok(Json(serde_json::json!({"stopped": true, "openid": openid})))
}

// ────────────────────────────────────────────────────────────────────
// /me/sop/tasks — list current user's SOP tasks (30-day retention)
// ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SopTasksQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    sop_name: Option<String>,
    #[serde(default = "default_sop_tasks_limit")]
    limit: i64,
}

fn default_sop_tasks_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct SopTaskView {
    task_id: String,
    sop_name: String,
    sop_name_cn: String,
    enterprise_name: Option<String>,
    status: String,
    deeplink: Option<String>,
    error: Option<String>,
    estimated_seconds: i64,
    created_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize)]
struct SopTasksResp {
    tasks: Vec<SopTaskView>,
    total: i64,
    has_more: bool,
}

async fn list_my_sop_tasks(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
    Query(q): Query<SopTasksQuery>,
) -> std::result::Result<Json<SopTasksResp>, Error> {
    let (rows, total) = sop_tasks::list_for_user(
        &st.pool,
        &openid,
        q.status.as_deref(),
        q.sop_name.as_deref(),
        q.limit,
    )
    .await?;

    let has_more = (rows.len() as i64) < total;

    let tasks = rows
        .into_iter()
        .map(|t| {
            let sop_name_cn = st
                .cfg
                .sop_metadata
                .get(&t.sop_name)
                .map(|m| m.display_name_cn.clone())
                .unwrap_or_else(|| t.sop_name.clone());
            SopTaskView {
                task_id: t.task_id,
                sop_name: t.sop_name,
                sop_name_cn,
                enterprise_name: t.enterprise_name,
                status: t.status,
                deeplink: t.deeplink,
                error: t.error_message,
                estimated_seconds: t.estimated_seconds,
                created_at: t.created_at,
                completed_at: t.completed_at,
            }
        })
        .collect();

    Ok(Json(SopTasksResp {
        tasks,
        total,
        has_more,
    }))
}

// ────────────────────────────────────────────────────────────────────
// POST /internal/sop-event — zeroclaw daemon lifecycle webhook
//
// Payload (JSON):
//   { "event": "starting", "sop_name": "...", "openid": "..." }
//   { "event": "done",     "sop_name": "...", "openid": "...", "response_text": "..." }
//
// The daemon fires "starting" when sop_execute tool is called (allows
// immediate task creation + SSE "created" before the run finishes).
// It fires "done" after the full agent loop completes with the
// response_text, which may contain a deeplink we extract.
// ────────────────────────────────────────────────────────────────────

/// Fallback: when response_text doesn't contain a parseable deeplink, try to
/// read `qualification_enterprise_id` from the workspace profile.json that the
/// SOP wrote in step 1. Avoids depending on LLM to format the link correctly.
async fn try_deeplink_from_workspace(
    cfg: &crate::config::Config,
    linux_uid: &str,
    enterprise_name: &str,
    sop_name: &str,
) -> Option<String> {
    if sop_name != "policy-match" {
        return None;
    }
    let enterprise_safe: String = enterprise_name
        .chars()
        .map(|c| if " /\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let profile_path = format!(
        "/home/{}/.zeroclaw/workspace/case/policy-match/{}/profile.json",
        linux_uid, enterprise_safe
    );
    let text = tokio::fs::read_to_string(&profile_path).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let qid = v.get("qualification_enterprise_id")?.as_i64()?;
    let tpl = &cfg.policy_match.mini_program_detail_path_template;
    Some(tpl.replace("{qualification_enterprise_id}", &qid.to_string()))
}

#[derive(Deserialize)]
struct SopEventPayload {
    event: String,
    sop_name: String,
    openid: Option<String>,
    #[serde(default)]
    response_text: Option<String>,
}

async fn internal_sop_event(
    State(st): State<AppState>,
    Json(payload): Json<SopEventPayload>,
) -> std::result::Result<Json<serde_json::Value>, Error> {
    let openid = match payload.openid.as_deref().filter(|s| !s.is_empty()) {
        Some(o) => o.to_string(),
        None => {
            tracing::warn!("sop-event missing openid — ignored");
            return Ok(Json(serde_json::json!({"ok": false, "reason": "missing openid"})));
        }
    };

    match payload.event.as_str() {
        "starting" => {
            // Task creation is owned by /chat (after enterprise-name validation).
            // Here we transition pending → running so the UI shows "进行中".
            // Race: zeroclaw fires this webhook before /chat finishes inserting the
            // pending row. Retry once after a short delay before falling back to
            // creating a running task directly.
            let task_id = sop_tasks::find_pending_by_sop(&st.pool, &openid, &payload.sop_name).await?;
            let task_id = if task_id.is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                sop_tasks::find_pending_by_sop(&st.pool, &openid, &payload.sop_name).await?
            } else {
                task_id
            };
            if let Some(task_id) = task_id {
                sop_tasks::mark_running(&st.pool, &task_id).await?;
                emit_sop_event(&st, &task_id, &openid, "running");
                tracing::debug!(openid = %openid, task_id = %task_id, "sop webhook: starting → running");
            } else {
                // Still no pending task — create a running row directly so the UI
                // can show "进行中". /chat may create a duplicate later; the done
                // handler will find whichever row was created first.
                let task_id = sop_tasks::new_task_id();
                let sop_meta = st.cfg.sop_metadata.get(payload.sop_name.as_str());
                let estimated_seconds = sop_meta.map(|m| m.estimated_seconds).unwrap_or(540);
                if let Err(e) = sop_tasks::insert_pending(
                    &st.pool, &task_id, &openid, &payload.sop_name, None, estimated_seconds,
                ).await {
                    tracing::warn!(openid = %openid, task_id = %task_id, "failed to insert fallback running task: {e}");
                } else {
                    sop_tasks::mark_running(&st.pool, &task_id).await?;
                    emit_sop_event(&st, &task_id, &openid, "running");
                    tracing::debug!(openid = %openid, task_id = %task_id, sop_name = %payload.sop_name,
                        "sop webhook: starting — created fallback running task");
                }
            }
            Ok(Json(serde_json::json!({"ok": true})))
        }
        "done" => {
            let response_text = payload.response_text.as_deref().unwrap_or("");
            let task_id = sop_tasks::find_pending_by_sop(
                &st.pool,
                &openid,
                &payload.sop_name,
            )
            .await?;
            let task_id = match task_id {
                Some(id) => id,
                None => {
                    // No pending task found — create a done row directly (e.g.
                    // daemon was restarted and clawops missed the "starting" event).
                    let id = sop_tasks::new_task_id();
                    tracing::info!(
                        openid = %openid,
                        task_id = %id,
                        sop_name = %payload.sop_name,
                        "sop webhook: done without prior pending — inserting done directly"
                    );
                    let (deeplink, _qid) = sop_tasks::extract_deeplink_and_qid(response_text);
                    sop_tasks::insert_cached_done(
                        &st.pool,
                        &id,
                        &openid,
                        &payload.sop_name,
                        None,
                        None,
                        None,
                        deeplink.as_deref(),
                        Some(response_text),
                        0,
                    )
                    .await?;
                    emit_sop_event(&st, &id, &openid, "created");
                    emit_sop_event(&st, &id, &openid, "done");
                    return Ok(Json(serde_json::json!({"ok": true, "task_id": id})));
                }
            };

            let (mut deeplink, _qid) = sop_tasks::extract_deeplink_and_qid(response_text);
            if deeplink.is_none() {
                // LLM didn't include a parseable link in response_text (e.g. gave a
                // conversational wrap-up instead of the step-6 format). Fall back to
                // reading qualification_enterprise_id directly from the workspace file.
                if let Ok(Some(task)) = sop_tasks::get_by_id(&st.pool, &task_id).await {
                    if let (Some(ename), Ok(Some(user))) = (
                        task.enterprise_name.as_deref(),
                        users::get(&st.pool, &openid).await,
                    ) {
                        deeplink = try_deeplink_from_workspace(
                            &st.cfg,
                            &user.linux_uid,
                            ename,
                            &payload.sop_name,
                        )
                        .await;
                    }
                }
            }
            tracing::info!(
                openid = %openid,
                task_id = %task_id,
                sop_name = %payload.sop_name,
                deeplink = ?deeplink,
                "sop webhook: done"
            );
            sop_tasks::mark_done(
                &st.pool,
                &task_id,
                None,
                None,
                deeplink.as_deref(),
                response_text,
            )
            .await?;
            emit_sop_event(&st, &task_id, &openid, "done");
            Ok(Json(serde_json::json!({"ok": true, "task_id": task_id})))
        }
        other => {
            tracing::warn!(event = other, "sop-event unknown event type — ignored");
            Ok(Json(serde_json::json!({"ok": false, "reason": "unknown event"})))
        }
    }
}


/// Map an internal SOP name to a short Chinese trigger phrase the LLM recognises.
fn sop_trigger_phrase(sop_name: &str) -> &'static str {
    match sop_name {
        "policy-match" => "帮我匹配政策",
        "qualification-check" => "帮我查资质",
        _ => "帮我执行",
    }
}

/// Return true when the message explicitly declares a (possibly new) company,
/// e.g. "我是拓尔思" / "换成华为" / "公司叫字节跳动".
/// Used to prevent stale memory company names from bleeding into fresh SOP triggers.
fn message_asserts_new_company(text: &str) -> bool {
    const ASSERTION_PATTERNS: &[&str] = &[
        "我是", "我们是", "我的公司是", "公司名是", "公司叫", "公司是",
        "换成", "换为", "改成", "改为",
    ];
    ASSERTION_PATTERNS.iter().any(|p| text.contains(p))
}

/// Return true if `name` looks like a full Chinese company legal name.
/// True if the message is a short re-run command with no new company context.
fn looks_like_full_company_name(name: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "有限公司", "有限责任公司", "股份有限公司", "股份合作公司",
        "集团有限公司", "集团股份有限公司", "合伙企业", "研究院", "研究所",
    ];
    let ch_count = name.chars().count();
    ch_count >= 6 && SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Scan free-form text for a substring that looks like a full company name.
/// Returns the first match found, or None.
fn extract_company_name_from_text(text: &str) -> Option<String> {
    const SUFFIXES: &[&str] = &[
        "有限公司", "有限责任公司", "股份有限公司", "股份合作公司",
        "集团有限公司", "集团股份有限公司", "合伙企业", "研究院", "研究所",
    ];
    // Full-width parentheses （） are intentionally excluded: Chinese company
    // names frequently embed region info as "中拓产业云（北京）科技服务有限公司".
    // Treating them as delimiters would truncate the name to "科技服务有限公司".
    const DELIMITERS: &[char] = &[
        ' ', '　', ',', '，', '。', '！', '？', '\n', '(', ')',
        '"', '"', '「', '」', '【', '】', ':', '：',
    ];
    let chars: Vec<char> = text.chars().collect();
    for suffix in SUFFIXES {
        let suffix_chars: Vec<char> = suffix.chars().collect();
        let slen = suffix_chars.len();
        let n = chars.len();
        if n < slen { continue; }
        for i in 0..=(n - slen) {
            if chars[i..i + slen] != suffix_chars[..] {
                continue;
            }
            let end = i + slen;
            // Walk backwards up to 20 chars to find the name start
            let look_back = end.saturating_sub(20);
            let name_start = (look_back..i)
                .rev()
                .find(|&j| DELIMITERS.contains(&chars[j]))
                .map(|j| j + 1)
                .unwrap_or(look_back);
            let name: String = chars[name_start..end].iter().collect();
            if looks_like_full_company_name(&name) {
                return Some(name);
            }
        }
    }
    None
}

fn sanitize_assistant_response(text: &str) -> String {
    sanitize_qualification_success_response(text).unwrap_or_else(|| text.to_string())
}

fn sanitize_qualification_success_response(text: &str) -> Option<String> {
    let deeplink = extract_qualification_deeplink(text)?;
    let has_internal_marker = [
        "HTTP 200",
        "data.wechat_page_url",
        "wechat_page_url",
        "按照 skill",
        "Step 3",
        "按格式回复",
        "链接构造",
        "SOUL 契约",
        "字段",
        "响应",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    let has_uncanonical_link = text.contains("pages/qualification/index.html?id=");

    if !has_internal_marker && !has_uncanonical_link {
        return None;
    }

    let subject = extract_qualification_company(text)
        .map(|name| format!("**{name}**"))
        .unwrap_or_else(|| "该企业".to_string());
    Some(format!(
        "已为 {subject} 触发资质数据处理，点击查看详情：\n\n\
         [查看企业资质详情]({deeplink})\n\n\
         详情页包含企业资质现状、到期提醒和可申报资质建议。如需深度资质分析（天眼查数据同步 + 专业申报路径规划），告诉我即可启动完整流程。"
    ))
}

fn extract_qualification_deeplink(text: &str) -> Option<String> {
    let re = Regex::new(r"/?pages/qualification/index(?:\.html)?\?id=(\d+)").ok()?;
    let caps = re.captures(text)?;
    let id = caps.get(1)?.as_str();
    Some(format!("/pages/qualification/index?id={id}"))
}

fn extract_qualification_company(text: &str) -> Option<String> {
    let re = Regex::new(r"已为\s+\*\*([^*\n]+)\*\*\s*触发资质").ok()?;
    if let Some(caps) = re.captures(text) {
        return caps.get(1).map(|m| m.as_str().trim().to_string());
    }
    extract_company_name_from_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_internal_qualification_success_output() {
        let raw = "HTTP 200 成功。按照 skill 的 Step 3 处理响应：\n\n\
            从 `data.wechat_page_url` 取详情页路径：`pages/qualification/index.html?id=127`\n\
            去掉 `.html`、补 `/` 前缀 → `/pages/qualification/index?id=127`\n\n\
            按格式回复：\n\
            已为 **中拓产业云（北京）科技服务有限公司** 触发资质数据处理，点击查看详情：\n\n\
            [查看企业资质详情](/pages/qualification/index?id=127)\n\n\
            检查 SOUL 契约：没有裸露内部主键。";

        let clean = sanitize_assistant_response(raw);

        assert!(clean.starts_with("已为 **中拓产业云（北京）科技服务有限公司** 触发资质数据处理"));
        assert!(clean.contains("[查看企业资质详情](/pages/qualification/index?id=127)"));
        assert!(!clean.contains("HTTP 200"));
        assert!(!clean.contains("wechat_page_url"));
        assert!(!clean.contains("Step 3"));
        assert!(!clean.contains("SOUL"));
    }

    #[test]
    fn normalizes_html_qualification_link_without_rewriting_clean_links() {
        let raw = "已为 **中拓产业云（北京）科技服务有限公司** 触发资质数据处理，点击查看详情：\n\n\
            [查看企业资质详情](pages/qualification/index.html?id=127)\n\n\
            详情页包含企业资质现状、到期提醒和可申报资质建议。";

        let clean = sanitize_assistant_response(raw);

        assert!(clean.contains("[查看企业资质详情](/pages/qualification/index?id=127)"));
        assert!(!clean.contains(".html?id=127"));

        let already_clean = "已为 **中拓产业云（北京）科技服务有限公司** 触发资质数据处理，点击查看详情：\n\n\
            [查看企业资质详情](/pages/qualification/index?id=127)\n\n\
            详情页包含企业资质现状、到期提醒和可申报资质建议。";
        assert_eq!(sanitize_assistant_response(already_clean), already_clean);
    }
}

// ── Qualification reminder endpoints ─────────────────────────────────────────

/// POST /internal/qualification-reminders
/// Called by zeroclaw SOP (qualification-check) after extracting expiry dates.
/// Clears existing pending reminders for the enterprise first, then inserts
/// the fresh batch so re-runs don't create duplicate notifications.
async fn internal_qualification_reminders(
    State(st): State<AppState>,
    Json(req): Json<qualification_reminders::CreateRemindersReq>,
) -> std::result::Result<Json<serde_json::Value>, Error> {
    qualification_reminders::clear_pending(&st.pool, &req.openid, &req.enterprise_name).await?;
    let n = qualification_reminders::insert_batch(&st.pool, &req).await?;
    tracing::info!(
        openid = %req.openid,
        enterprise = %req.enterprise_name,
        count = n,
        "qualification reminders registered"
    );
    Ok(Json(serde_json::json!({"ok": true, "registered": n})))
}

/// GET /me/qualification-reminders — list the current user's qualification reminders.
async fn list_my_qualification_reminders(
    State(st): State<AppState>,
    AuthOpenid(openid): AuthOpenid,
) -> std::result::Result<Json<serde_json::Value>, Error> {
    let rows = qualification_reminders::list_for_user(&st.pool, &openid).await?;
    Ok(Json(serde_json::json!({"reminders": rows})))
}

/// Background task: scan due qualification reminders every 5 minutes,
/// dispatch SMS (mini-program users) or webhook (wecom uin: users).
pub fn spawn_qualification_reminder_cron(
    pool: sqlx::SqlitePool,
    cfg: Arc<Config>,
    http: reqwest::Client,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let due = match qualification_reminders::fetch_due(&pool).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("qual reminder scan error: {e}");
                    continue;
                }
            };
            for r in due {
                match qualification_reminders::dispatch(&pool, &http, &cfg.qualification, &r).await {
                    Ok(()) => {
                        let _ = qualification_reminders::mark_sent(&pool, r.id).await;
                        tracing::info!(
                            id = r.id, openid = %r.openid,
                            title = %r.qualification_title,
                            "qual reminder sent"
                        );
                    }
                    Err(reason) => {
                        let _ = qualification_reminders::mark_failed(&pool, r.id, &reason).await;
                        tracing::warn!(
                            id = r.id, openid = %r.openid,
                            title = %r.qualification_title,
                            reason = %reason,
                            "qual reminder failed"
                        );
                    }
                }
            }
        }
    });
}

/// Background task: timeout stale running/pending SOP tasks every 5 minutes.
/// A task is considered stale if it has been in running/pending for > 30 min
/// without a done event (zeroclaw webhook dropped or daemon crashed mid-SOP).
pub fn spawn_sop_task_watchdog(pool: sqlx::SqlitePool) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            match sop_tasks::timeout_stale(&pool, 30).await {
                Ok(n) if n > 0 => tracing::warn!("sop watchdog: timed out {n} stale task(s)"),
                Ok(_) => {}
                Err(e) => tracing::error!("sop watchdog error: {e}"),
            }
        }
    });
}

