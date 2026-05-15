use crate::auth::WxClient;
use crate::config::Config;
use crate::intent_classifier::IntentClassifier;
use crate::limits::{self, AppLimiters};
use crate::provisioner::Provisioner;
use crate::sop_runner;
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
    /// LLM-based intent classifier. Wrapped in Arc for cheap clone in
    /// each chat handler invocation. None when feature disabled.
    pub intent_classifier: Arc<IntentClassifier>,
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
        .route("/admin/users", get(list_users))
        .route("/admin/users/:openid", get(get_user))
        .route("/admin/provision", post(admin_provision))
        .route("/admin/stop/:openid", post(admin_stop))
        .route("/admin/issue-token", post(admin_issue_token))
        .route("/admin/refresh-workspace/:openid", post(admin_refresh_workspace))
        .route("/admin/refresh-all-workspaces", post(admin_refresh_all_workspaces))
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
    // Rate-limit by openid (defends LLM-cost blast radius if a token leaks)
    limits::check(&st.limiters.chat_per_user, &openid)
        .map_err(|retry| Error::RateLimited { retry_after_secs: retry })?;

    let user = st.provisioner.ensure_running(&openid).await?;
    users::touch_active(&st.pool, &user.openid).await?;

    // ── Intent classification ─────────────────────────────────────
    // Run the LLM classifier on the user message. On any failure or
    // confidence below threshold, fall through to the legacy sync
    // daemon path (normal_chat). On SOP hit, return immediately with
    // a "task queued" message and let sop_runner do the work.
    let classify = st.intent_classifier.classify(&req.content).await;
    let min_conf = st.cfg.intent_classifier.min_confidence;
    let sop_match = if classify.is_sop_trigger(min_conf) {
        st.cfg.sop_metadata.get(&classify.intent).cloned()
            .map(|m| (classify.intent.clone(), m))
    } else {
        None
    };

    if let Some((sop_name, sop_meta)) = sop_match {
        // Best-effort enterprise_name extraction from user profile
        // (the SOP itself will run its own resolution chain — this is
        // just for cache lookup + task display).
        let enterprise_name = users::get(&st.pool, &user.openid)
            .await
            .ok()
            .flatten()
            .and_then(|u| u.enterprise_profile)
            .and_then(|s| serde_json::from_str::<JsonValue>(&s).ok())
            .and_then(|v| v.get("company_name").and_then(|x| x.as_str()).map(String::from));

        // Cache check (per-openid, sop_name + enterprise_name, within ttl)
        let cached = sop_tasks::find_cached_done(
            &st.pool,
            &user.openid,
            &sop_name,
            enterprise_name.as_deref(),
            sop_meta.cache_ttl_days,
        )
        .await
        .ok()
        .flatten();

        let task_id = sop_tasks::new_task_id();
        let display_name_cn = sop_meta.display_name_cn.clone();

        let chat_response_text = if let Some(prev) = cached {
            // Cache hit: copy deeplink + ids into a fresh row at status=done.
            sop_tasks::insert_cached_done(
                &st.pool,
                &task_id,
                &user.openid,
                &sop_name,
                enterprise_name.as_deref(),
                prev.enterprise_id,
                prev.qualification_enterprise_id,
                prev.deeplink.as_deref(),
                prev.response_text.as_deref(),
                sop_meta.estimated_seconds,
            )
            .await?;
            // Emit SSE: created (with sub-event "done" implied next)
            emit_sop_event(&st, &task_id, &user.openid, "created");
            emit_sop_event(&st, &task_id, &user.openid, "done");
            format!(
                "已为您找到上次「{}」结果,可在右上角任务列表查看",
                display_name_cn
            )
        } else {
            // Cache miss: insert pending + spawn background runner.
            sop_tasks::insert_pending(
                &st.pool,
                &task_id,
                &user.openid,
                &sop_name,
                enterprise_name.as_deref(),
                sop_meta.estimated_seconds,
            )
            .await?;
            // Emit "created" now so the frontend can immediately show
            // the task in the list (enterprise_name already stored above).
            emit_sop_event(&st, &task_id, &user.openid, "created");
            sop_runner::spawn(sop_runner::SopRunCtx {
                pool: st.pool.clone(),
                http: st.http.clone(),
                provisioner: st.provisioner.clone(),
                cfg: st.cfg.clone(),
                sop_event_tx: st.sop_event_tx.clone(),
                task_id: task_id.clone(),
                openid: user.openid.clone(),
                sop_name: sop_name.clone(),
                user_message: req.content.clone(),
            });
            let minutes = (sop_meta.estimated_seconds + 59) / 60;
            format!(
                "已开始「{}」,预计 {} 分钟,可在右上角任务列表查看进度",
                display_name_cn, minutes
            )
        };

        // Persist chat turn so frontend onLoad can re-render
        if let Err(e) = chat_history::record_turn(
            &st.pool,
            &user.openid,
            &req.content,
            &chat_response_text,
        )
        .await
        {
            tracing::warn!(openid = %user.openid, "failed to persist chat turn: {e}");
        }

        return Ok(Json(ChatResp {
            response: chat_response_text,
            model: Some("clawops-router".to_string()),
            openid: user.openid,
        }));
    }

    // ── Normal chat path (no SOP triggered) ───────────────────────
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
            .json(&serde_json::json!({"message": req.content}));
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
