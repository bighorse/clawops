//! Rate limiting backed by `governor`.
//!
//! Three keyed limiters live for the process lifetime:
//! - `wx_login_per_ip`: throttles `/auth/wx-login` per source IP. Defends
//!   against brute-force attempts to enumerate openids by spamming codes.
//! - `chat_per_user`: throttles `/chat` per session-resolved openid. Cuts
//!   the LLM cost-blast radius if a user's token leaks or a buggy client
//!   loops on send.
//! - `admin_per_ip`: throttles `/admin/*` per source IP as a second line
//!   behind the admin token (for blind scanning of the surface).
//!
//! Source IP is read from the first hop in `X-Forwarded-For`. ClawOps is
//! deployed behind a trusted nginx that always sets this; production ufw
//! rules ensure nothing else can reach :8088, so the header is trusted.

use crate::config::RateLimitConfig;
use axum::http::HeaderMap;
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;
use std::sync::Arc;

type KeyedLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

pub struct AppLimiters {
    pub wx_login_per_ip: Arc<KeyedLimiter>,
    pub chat_per_user: Arc<KeyedLimiter>,
    pub admin_per_ip: Arc<KeyedLimiter>,
}

impl AppLimiters {
    pub fn new(cfg: &RateLimitConfig) -> Self {
        Self {
            wx_login_per_ip: Arc::new(RateLimiter::keyed(quota_per_min(
                cfg.wx_login_per_ip_per_min,
            ))),
            chat_per_user: Arc::new(RateLimiter::keyed(quota_per_min(
                cfg.chat_per_user_per_min,
            ))),
            admin_per_ip: Arc::new(RateLimiter::keyed(quota_per_min(
                cfg.admin_per_ip_per_min,
            ))),
        }
    }
}

fn quota_per_min(n: u32) -> Quota {
    Quota::per_minute(NonZeroU32::new(n.max(1)).unwrap())
}

/// Read the originating client IP. Trusts `X-Forwarded-For` first hop;
/// if absent, falls back to `"unknown"` (single bucket for direct hits —
/// in production the firewall makes that path impossible).
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            headers
                .get("X-Real-IP")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".into())
        })
}

/// Run a check; on rejection, return `Some(retry_after_secs)`.
pub fn check(limiter: &KeyedLimiter, key: &str) -> Result<(), u64> {
    match limiter.check_key(&key.to_string()) {
        Ok(()) => Ok(()),
        Err(neg) => {
            let now = DefaultClock::default().now();
            let wait = neg.wait_time_from(now);
            Err(wait.as_secs().max(1))
        }
    }
}
