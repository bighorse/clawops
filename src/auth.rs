//! WeChat mini-program login (`wx.login` → `code2session`).
//!
//! When `wx.appid` is empty in clawops.toml the client runs in **mock**
//! mode — the request body's `openid` field is trusted directly. This is
//! intended for local development; in production the mock branch must be
//! disabled by populating wx.appid + wx.secret.

use crate::config::WxConfig;
use crate::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Code2SessionResp {
    /// Optional because WeChat omits it in error responses
    /// (e.g. invalid appid returns only errcode + errmsg).
    #[serde(default)]
    pub openid: String,
    #[serde(default)]
    pub unionid: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
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
        self.cfg.appid.is_empty() || self.cfg.secret.is_empty()
    }

    /// Exchange the wx.login `code` for an openid. In mock mode, the
    /// `mock_openid` parameter is returned directly. In production mode
    /// (wx.appid + wx.secret both set) the `mock_openid` field MUST be
    /// absent — passing it returns DevFieldInProd to make config drift
    /// loud rather than silently allow openid spoofing.
    pub async fn code2session(
        &self,
        code: &str,
        mock_openid: Option<&str>,
    ) -> Result<Code2SessionResp> {
        let mock_supplied = mock_openid.map(|s| !s.is_empty()).unwrap_or(false);

        if self.is_mock() {
            let openid = mock_openid
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::Other(
                        "wx.appid empty and no mock_openid supplied".into(),
                    )
                })?
                .to_string();
            return Ok(Code2SessionResp {
                openid,
                unionid: None,
                session_key: None,
                errcode: Some(0),
                errmsg: Some("mock".into()),
            });
        }

        // Production guard: refuse mock_openid so a misconfigured deployment
        // doesn't accidentally let clients spoof identity.
        if mock_supplied {
            return Err(Error::DevFieldInProd("mock_openid"));
        }

        if code.is_empty() {
            return Err(Error::WxApiError {
                errcode: -10001,
                errmsg: "empty code (call wx.login first)".into(),
            });
        }

        let url = format!(
            "https://api.weixin.qq.com/sns/jscode2session?appid={}&secret={}&js_code={}&grant_type=authorization_code",
            self.cfg.appid, self.cfg.secret, code
        );
        let resp: Code2SessionResp = self.http.get(&url).send().await?.json().await?;
        if resp.errcode.unwrap_or(0) != 0 {
            return Err(Error::WxApiError {
                errcode: resp.errcode.unwrap_or(-1),
                errmsg: resp.errmsg.unwrap_or_else(|| "unknown".into()),
            });
        }
        if resp.openid.is_empty() {
            return Err(Error::WxApiError {
                errcode: -10002,
                errmsg: "wechat returned empty openid".into(),
            });
        }
        Ok(resp)
    }
}
