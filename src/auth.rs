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
    /// `mock_openid` parameter is returned directly (callers must supply
    /// one in dev).
    pub async fn code2session(
        &self,
        code: &str,
        mock_openid: Option<&str>,
    ) -> Result<Code2SessionResp> {
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

        let url = format!(
            "https://api.weixin.qq.com/sns/jscode2session?appid={}&secret={}&js_code={}&grant_type=authorization_code",
            self.cfg.appid, self.cfg.secret, code
        );
        let resp: Code2SessionResp = self.http.get(&url).send().await?.json().await?;
        if resp.errcode.unwrap_or(0) != 0 {
            return Err(Error::Other(format!(
                "code2session failed: errcode={:?} errmsg={:?}",
                resp.errcode, resp.errmsg
            )));
        }
        Ok(resp)
    }
}
