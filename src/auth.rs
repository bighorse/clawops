//! WeChat mini-program login — code → openid exchange.
//!
//! ClawOps does **not** call `jscode2session` directly. Instead it POSTs
//! the wx.login code to the platform's exchange endpoint, which performs
//! the WeChat call using its own `access_token`. This avoids the
//! single-use `code` being consumed twice and removes the need for
//! ClawOps to hold the WeChat AppSecret.
//!
//! Endpoint contract (handled by `bdhrapi.2048office.com`):
//!   POST {backend_base_url}/message/wechat/applets/{app_id}/open_id
//!   Content-Type: application/json
//!   Body:  {"code": "<wx code>", "client": "clawops"}
//!   200:   {"request_id": "...", "message": "...", "data": {"open_id": "..."}}
//!   403:   unconfigured app_id ({"message": "未配置该公众平台", ...})
//!   500:   code expired / already used
//!
//! Mock mode: when `backend_base_url` is empty (local dev / macOS), the
//! request body's `mock_openid` is trusted directly. Setting
//! `backend_base_url` automatically rejects `mock_openid` to prevent
//! identity spoofing on production.

use crate::config::WxConfig;
use crate::{Error, Result};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Code2SessionResp {
    pub openid: String,
    /// Breed named by the exchange backend, when it names one.
    ///
    /// The backend already knows which mini-program the code came from,
    /// so letting it answer "and this one is a `shangji` tenant" keeps
    /// breed ownership in one system instead of two. Optional on the wire:
    /// a backend that never sends it behaves exactly as before, and
    /// ClawOps falls back to `provisioner.breed_routes` and then to
    /// `default_breed`.
    pub breed: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExchangeResp {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<ExchangeData>,
}

#[derive(Debug, Deserialize)]
struct ExchangeData {
    #[serde(default)]
    open_id: String,
    /// Optional. See `Code2SessionResp::breed`.
    #[serde(default)]
    breed: Option<String>,
}

pub struct WxClient {
    cfg: WxConfig,
    http: reqwest::Client,
}

impl WxClient {
    pub fn new(cfg: WxConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    pub fn is_mock(&self) -> bool {
        self.cfg.backend_base_url.is_empty()
    }

    /// Exchange wx.login `code` for an openid via the platform backend.
    ///
    /// `app_id` identifies which mini-program the code came from — the
    /// backend rejects (403) any app_id not configured on its side, so
    /// ClawOps doesn't need its own whitelist. `mock_openid` is honored
    /// only in mock mode and rejected otherwise.
    pub async fn code2session(
        &self,
        app_id: &str,
        code: &str,
        mock_openid: Option<&str>,
    ) -> Result<Code2SessionResp> {
        let mock_supplied = mock_openid.map(|s| !s.is_empty()).unwrap_or(false);

        if self.is_mock() {
            let openid = mock_openid
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::Other(
                        "wx.backend_base_url empty and no mock_openid supplied".into(),
                    )
                })?
                .to_string();
            return Ok(Code2SessionResp { openid, breed: None });
        }

        if mock_supplied {
            return Err(Error::DevFieldInProd("mock_openid"));
        }

        if app_id.is_empty() {
            return Err(Error::WxApiError {
                errcode: -10003,
                errmsg: "missing app_id".into(),
            });
        }
        if code.is_empty() {
            return Err(Error::WxApiError {
                errcode: -10001,
                errmsg: "empty code (call wx.login first)".into(),
            });
        }

        let url = format!(
            "{}/message/wechat/applets/{}/open_id",
            self.cfg.backend_base_url.trim_end_matches('/'),
            app_id
        );
        let resp = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(self.cfg.exchange_timeout_secs))
            .json(&serde_json::json!({"code": code, "client": "clawops"}))
            .send()
            .await?;
        let status = resp.status();
        let body: ExchangeResp = resp.json().await.map_err(|e| Error::WxApiError {
            errcode: -10004,
            errmsg: format!("backend returned non-JSON body: {e}"),
        })?;
        if !status.is_success() {
            return Err(Error::WxApiError {
                errcode: status.as_u16() as i64,
                errmsg: body.message.unwrap_or_else(|| "backend error".into()),
            });
        }
        let data = body.data.filter(|d| !d.open_id.is_empty()).ok_or_else(|| {
            Error::WxApiError {
                errcode: -10002,
                errmsg: "backend returned empty open_id".into(),
            }
        })?;
        Ok(Code2SessionResp {
            openid: data.open_id,
            // An empty string is the same as absent — a backend that sends
            // `"breed": ""` for "no opinion" must not push the tenant onto a
            // breed literally named "".
            breed: data.breed.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()),
        })
    }
}
