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
        let (status, msg) = match &self {
            Error::UserNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::UserAlreadyExists(_) => (StatusCode::CONFLICT, self.to_string()),
            Error::NoFreePort => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            _ => {
                tracing::error!(error = %self, "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
        };
        (status, axum::Json(serde_json::json!({"error": msg}))).into_response()
    }
}
