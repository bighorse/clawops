/// WeChat subscribeMessage.send integration.
///
/// access_token 有效期 7200s，用 tokio::Mutex 缓存，到期前 60s 主动刷新。
use crate::config::WxNotifyConfig;
use crate::Result;
use anyhow::anyhow;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct WxNotifier {
    cfg: WxNotifyConfig,
    client: Client,
    token_cache: Arc<Mutex<TokenCache>>,
}

#[derive(Debug, Default)]
struct TokenCache {
    token: String,
    expires_at: i64, // unix timestamp
}

impl WxNotifier {
    pub fn new(cfg: WxNotifyConfig) -> Self {
        Self {
            cfg,
            client: Client::new(),
            token_cache: Arc::new(Mutex::new(TokenCache::default())),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.cfg.appid.is_empty() && !self.cfg.appsecret.is_empty()
    }

    async fn access_token(&self) -> Result<String> {
        let now = chrono::Utc::now().timestamp();
        {
            let cache = self.token_cache.lock().await;
            if cache.expires_at > now + 60 {
                return Ok(cache.token.clone());
            }
        }
        let url = format!(
            "https://api.weixin.qq.com/cgi-bin/token\
             ?grant_type=client_credential&appid={}&secret={}",
            self.cfg.appid, self.cfg.appsecret
        );
        let resp: Value = self.client.get(&url).send().await?.json().await?;
        if let Some(err) = resp.get("errmsg").and_then(|v| v.as_str()) {
            if err != "ok" {
                return Err(anyhow!("wx token error: {}", resp).into());
            }
        }
        let token = resp["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("wx token missing access_token field"))?
            .to_string();
        let expires_in = resp["expires_in"].as_i64().unwrap_or(7200);
        {
            let mut cache = self.token_cache.lock().await;
            cache.token = token.clone();
            cache.expires_at = now + expires_in;
        }
        Ok(token)
    }

    /// Send 活动开始提醒 subscription message.
    /// `remind_page` should include `?obj_id=<activity_id>`.
    pub async fn send_activity_remind(
        &self,
        openid: &str,
        activity_name: &str,
        activity_time: &str,
        activity_venue: &str,
        activity_id: &str,
    ) -> Result<()> {
        if !self.is_configured() {
            tracing::warn!("wx_notify not configured — skipping send");
            return Ok(());
        }
        let token = self.access_token().await?;
        let page = format!("{}?obj_id={}", self.cfg.activity_remind_page, activity_id);
        let body = json!({
            "touser": openid,
            "template_id": self.cfg.activity_remind_template_id,
            "page": page,
            "miniprogram_state": "formal",
            "lang": "zh_CN",
            "data": {
                "thing1": { "value": truncate(activity_name, 20) },
                "time2":  { "value": activity_time },
                "thing3": { "value": truncate(activity_venue, 20) },
                "thing4": { "value": "活动即将开始，请做好准备" }
            }
        });
        let url = format!(
            "https://api.weixin.qq.com/cgi-bin/message/subscribe/send?access_token={}",
            token
        );
        let resp: Value = self.client.post(&url).json(&body).send().await?.json().await?;
        let errcode = resp["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            return Err(anyhow!(
                "subscribeMessage.send failed: errcode={} errmsg={}",
                errcode,
                resp["errmsg"].as_str().unwrap_or("?")
            )
            .into());
        }
        Ok(())
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let collected: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}…", collected)
    } else {
        collected
    }
}
