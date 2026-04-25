use crate::auth::WxClient;
use crate::config::Config;
use crate::provisioner::Provisioner;
use crate::{sessions, users, Error, Result};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub cfg: Arc<Config>,
    pub provisioner: Arc<Provisioner>,
    pub http: reqwest::Client,
    pub wx: Arc<WxClient>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/auth/wx-login", post(wx_login))
        .route("/chat", post(chat))
        .route("/events", get(events))
        .route("/admin/users", get(list_users))
        .route("/admin/users/:openid", get(get_user))
        .route("/admin/provision", post(admin_provision))
        .route("/admin/stop/:openid", post(admin_stop))
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

/// GET /events — SSE byte-stream proxy to the user's zeroclaw /api/events.
/// Auth via `Authorization: Bearer <session_token>` (or `?token=` for
/// EventSource clients that can't set headers). Each request opens its own
/// upstream connection (1:1); when the client disconnects, hyper drops the
/// stream and the upstream TCP connection is closed automatically. No SSE
/// parsing — bytes pass through unchanged.
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

    if !st.provisioner.backend.launches_daemon() {
        // Mock backend: no real upstream, emit a one-shot synthetic event
        // so end-to-end SSE plumbing on the client side can be exercised.
        let body = format!(
            "data: {{\"type\":\"mock_hello\",\"openid\":\"{}\"}}\n\n",
            user.openid
        );
        return Ok(([
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ], body)
            .into_response());
    }

    let port = user.port.ok_or_else(|| {
        Error::Other(format!("user {} has no port assigned", user.openid))
    })?;
    let token = user
        .paired_token_enc
        .as_deref()
        .ok_or_else(|| Error::Other("paired token missing".into()))?;

    let upstream = st
        .http
        .get(format!("http://127.0.0.1:{port}/api/events"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await?;

    if !upstream.status().is_success() {
        return Err(Error::Other(format!(
            "upstream /api/events returned {}",
            upstream.status()
        )));
    }

    let stream = upstream.bytes_stream();
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
    /// Code returned by `wx.login()` on the mini-program side.
    #[serde(default)]
    code: String,
    /// Mock openid used when wx.appid is empty (dev only).
    #[serde(default)]
    mock_openid: Option<String>,
    /// Optional phone number (from getPhoneNumber). Stored on first login.
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
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

async fn wx_login(
    State(st): State<AppState>,
    Json(req): Json<WxLoginReq>,
) -> std::result::Result<Json<WxLoginResp>, Error> {
    let session = st
        .wx
        .code2session(&req.code, req.mock_openid.as_deref())
        .await?;

    let openid = session.openid.clone();
    let mut is_new_user = false;
    if users::get(&st.pool, &openid).await?.is_none() {
        is_new_user = true;
        let new = users::NewUser {
            openid: openid.clone(),
            phone: req.phone,
            display_name: req.display_name,
            enterprise_profile: req.enterprise_profile,
        };
        st.provisioner.provision(&new).await?;
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
    let user = st.provisioner.ensure_running(&openid).await?;
    users::touch_active(&st.pool, &user.openid).await?;

    // Phase 1: if the backend does not actually launch a daemon, we return
    // a canned response for smoke-testing the pipeline. Real mode forwards
    // to the user's zeroclaw /webhook.
    if !st.provisioner.backend.launches_daemon() {
        return Ok(Json(ChatResp {
            response: format!("[mock] echo: {}", req.content),
            model: Some("mock".into()),
            openid: user.openid,
        }));
    }

    let port = user.port.ok_or_else(|| {
        Error::Other(format!("user {} has no port assigned", user.openid))
    })?;
    let url = format!("http://127.0.0.1:{port}/webhook");

    let mut builder = st
        .http
        .post(&url)
        .json(&serde_json::json!({"message": req.content}));

    if let Some(token) = &user.paired_token_enc {
        // In phase 1 the token is stored in plaintext; encryption is TODO.
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(idem) = &req.idempotency_key {
        builder = builder.header("X-Idempotency-Key", idem);
    }

    let resp = builder.send().await?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "zeroclaw /webhook returned {}",
            resp.status()
        )));
    }
    let body: serde_json::Value = resp.json().await?;
    Ok(Json(ChatResp {
        response: body
            .get("response")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        openid: user.openid,
    }))
}

async fn list_users(State(st): State<AppState>) -> Result<Json<Vec<users::User>>> {
    let rows: Vec<users::User> = sqlx::query_as(
        "SELECT * FROM users ORDER BY created_at DESC",
    )
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

async fn get_user(
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
    enterprise_profile: Option<serde_json::Value>,
}

async fn admin_provision(
    State(st): State<AppState>,
    Json(req): Json<ProvisionReq>,
) -> std::result::Result<impl IntoResponse, Error> {
    let new = users::NewUser {
        openid: req.openid,
        phone: req.phone,
        display_name: req.display_name,
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

async fn admin_stop(
    State(st): State<AppState>,
    Path(openid): Path<String>,
) -> std::result::Result<impl IntoResponse, Error> {
    st.provisioner.stop(&openid).await?;
    Ok(Json(serde_json::json!({"stopped": true, "openid": openid})))
}
