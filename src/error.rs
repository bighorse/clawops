use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("http client error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("template error: {0}")]
    Template(#[from] handlebars::RenderError),

    #[error("no free port in configured range")]
    NoFreePort,

    #[error("user not found: {0}")]
    UserNotFound(String),

    #[error("user already exists: {0}")]
    UserAlreadyExists(String),

    #[error("process backend error: {0}")]
    Process(String),

    #[error("zeroclaw not reachable on {host}:{port} after {waited_ms}ms")]
    ZeroclawNotReady {
        host: String,
        port: u16,
        waited_ms: u64,
    },

    /// A daemon answered on the port, but not the tenant's own: its authenticated
    /// probe was rejected. Almost always a port collision with a process ClawOps
    /// does not manage.
    #[error("port {port} is answering for a different daemon (probe returned {status})")]
    ZeroclawIdentityMismatch { port: u16, status: u16 },

    /// Surfaces WeChat code2session errors verbatim to the client so the
    /// mini-program can react (re-call wx.login on 40029, retry on 45011, etc.)
    #[error("wechat code2session failed: errcode={errcode} errmsg={errmsg}")]
    WxApiError { errcode: i64, errmsg: String },

    /// Client tried to use a dev-only field in production (e.g. `mock_openid`
    /// when wx.appid is configured).
    #[error("dev-only field used in production: {0}")]
    DevFieldInProd(&'static str),

    /// Per-IP / per-user rate limit exceeded.
    #[error("rate limit exceeded; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    /// Caller-supplied input is malformed — 400, not 500. Without this,
    /// validation failures fall through to the catch-all and look like
    /// server faults to the client.
    #[error("{0}")]
    BadRequest(String),

    /// Requested resource isn't there — or the caller isn't allowed to know
    /// whether it is. Artifact lookups deliberately answer 404 for traversal
    /// attempts too, so probing can't distinguish "blocked" from "absent".
    #[error("{0}")]
    NotFound(String),
    /// A tenant (or an admin request) named a breed with no template
    /// directory behind it. Never rendered from a fallback: handing a
    /// tenant the wrong breed's prompts is worse than refusing.
    #[error("unknown breed '{0}': no template directory for it")]
    UnknownBreed(String),

    /// An uploaded breed bundle was rejected before anything was written.
    #[error("invalid breed bundle: {0}")]
    BadBundle(String),

    /// Breed writes were attempted while `provisioner.breeds_dir` is unset.
    #[error("breeds_dir is not configured; ClawOps is in single-breed mode")]
    BreedsDisabled,

    /// A breed still has tenants rendering from it. Deleting its
    /// templates would freeze them on whatever is already in their
    /// workspace and turn their next refresh into a 404.
    #[error("breed '{breed}' still has {tenants} tenant(s); move them first via PUT /admin/users/<openid>/breed")]
    BreedInUse { breed: String, tenants: i64 },

    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(format!("{e:#}"))
    }
}

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        // WeChat-specific path returns structured details so the mini-program
        // can branch on errcode (40029 = re-login, 45011 = backoff, etc.)
        if let Error::RateLimited { retry_after_secs } = &self {
            let mut resp = (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(serde_json::json!({
                    "error": "rate_limited",
                    "retry_after_secs": retry_after_secs,
                })),
            )
                .into_response();
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                retry_after_secs.to_string().parse().unwrap(),
            );
            return resp;
        }
        if let Error::WxApiError { errcode, errmsg } = &self {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": "wechat_login_failed",
                    "errcode": errcode,
                    "errmsg": errmsg,
                })),
            )
                .into_response();
        }
        let (status, msg) = match &self {
            Error::UserNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::UserAlreadyExists(_) => (StatusCode::CONFLICT, self.to_string()),
            Error::NoFreePort => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Error::DevFieldInProd(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::UnknownBreed(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::BadBundle(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::BreedsDisabled => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Error::BreedInUse { .. } => (StatusCode::CONFLICT, self.to_string()),
            _ => {
                tracing::error!(error = %self, "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
        };
        (status, axum::Json(serde_json::json!({"error": msg}))).into_response()
    }
}
