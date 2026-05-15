//! LLM-based intent classifier for the SOP async task flow.
//!
//! Given a user chat message, decides whether it matches one of the
//! configured SOPs (e.g. `policy-match`, `qualification-check`) or is
//! a normal chat. Returns {intent, confidence}.
//!
//! Strategy: 1 LLM call to deepseek-v4-flash (or similar fast model)
//! with a structured prompt listing each SOP's intent_description.
//! Total latency ~1-2s, ~99% accuracy in practice. Result cached by
//! message hash for cache_ttl_secs (default 10 min) to avoid re-calls
//! when the user retries the same message.
//!
//! Fallback: any error (HTTP failure, JSON parse error, timeout)
//! returns `Intent::NormalChat` with confidence 0, which routes the
//! request to the daemon's native sync path — the user sees a 9-min
//! hang but no crash.

use crate::config::{IntentClassifierConfig, SopMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifyResult {
    /// Either a `sop_name` from sop_metadata or the literal `"normal_chat"`.
    pub intent: String,
    /// LLM-reported confidence (0.0-1.0).
    pub confidence: f32,
}

impl ClassifyResult {
    pub fn normal_chat() -> Self {
        Self {
            intent: "normal_chat".to_string(),
            confidence: 0.0,
        }
    }

    pub fn is_sop_trigger(&self, threshold: f32) -> bool {
        self.intent != "normal_chat" && self.confidence >= threshold
    }
}

#[derive(Clone)]
pub struct IntentClassifier {
    config: Arc<IntentClassifierConfig>,
    /// SOP metadata indexed by sop_name, used to build the prompt and
    /// validate that the LLM didn't return an unknown intent.
    sop_metadata: Arc<HashMap<String, SopMetadata>>,
    http: reqwest::Client,
    /// Cache: message hash → (inserted_at, result). Cleaned lazily on
    /// each lookup (cheap since map is tiny — bounded by traffic in
    /// cache_ttl_secs window).
    cache: Arc<Mutex<HashMap<String, (Instant, ClassifyResult)>>>,
}

impl IntentClassifier {
    pub fn new(
        config: IntentClassifierConfig,
        sop_metadata: HashMap<String, SopMetadata>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            config: Arc::new(config),
            sop_metadata: Arc::new(sop_metadata),
            http,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Classify a user message. Returns NormalChat on any error
    /// (logged, but doesn't propagate — caller routes to daemon sync).
    pub async fn classify(&self, message: &str) -> ClassifyResult {
        // Short-circuit: disabled, no key, or no SOPs configured
        if !self.config.enabled
            || self.config.api_key.is_empty()
            || self.sop_metadata.is_empty()
        {
            return ClassifyResult::normal_chat();
        }

        let cache_key = format!("{}:{}", self.config.model, message);
        // Cache hit
        if let Some(hit) = self.cache_get(&cache_key).await {
            return hit;
        }

        let result = match self.call_llm(message).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "intent classification failed, falling back to normal_chat");
                ClassifyResult::normal_chat()
            }
        };

        self.cache_put(cache_key, result.clone()).await;
        result
    }

    async fn cache_get(&self, key: &str) -> Option<ClassifyResult> {
        let mut cache = self.cache.lock().await;
        let ttl = Duration::from_secs(self.config.cache_ttl_secs);
        // Cleanup expired entries lazily
        cache.retain(|_, (t, _)| t.elapsed() < ttl);
        cache.get(key).map(|(_, r)| r.clone())
    }

    async fn cache_put(&self, key: String, result: ClassifyResult) {
        let mut cache = self.cache.lock().await;
        cache.insert(key, (Instant::now(), result));
        // Optional: bound size to avoid leaks under pathological traffic
        if cache.len() > 10_000 {
            cache.clear();
        }
    }

    async fn call_llm(&self, message: &str) -> anyhow::Result<ClassifyResult> {
        let prompt = self.build_prompt(message);
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": [{"role": "user", "content": prompt}],
            "response_format": {"type": "json_object"},
            "temperature": 0.0,
            "max_tokens": 80,
        });

        let resp = self
            .http
            .post(&self.config.api_url)
            .timeout(Duration::from_secs(self.config.timeout_secs))
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("classifier upstream {} returned: {}", status, json);
        }

        let content = json
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("classifier response missing content"))?;

        let parsed: ClassifyResult = serde_json::from_str(content)
            .map_err(|e| anyhow::anyhow!("classifier JSON parse: {}; raw: {}", e, content))?;

        // Validate the intent is either "normal_chat" or a known sop_name
        if parsed.intent != "normal_chat" && !self.sop_metadata.contains_key(&parsed.intent) {
            anyhow::bail!(
                "classifier returned unknown intent '{}' (configured: {:?})",
                parsed.intent,
                self.sop_metadata.keys().collect::<Vec<_>>()
            );
        }

        Ok(parsed)
    }

    fn build_prompt(&self, message: &str) -> String {
        let mut sop_list = String::new();
        for (name, meta) in self.sop_metadata.iter() {
            sop_list.push_str(&format!("- {}: {}\n", name, meta.intent_description));
        }
        format!(
            r#"你是企服小程序的意图分类器。判断用户消息属于以下哪种意图之一,仅输出 JSON。

可选意图:
{}- normal_chat: 其他普通对话、咨询、闲聊、打招呼

输出格式(严格 JSON,不要 markdown,不要多余文字):
{{"intent": "<意图名>", "confidence": 0.0-1.0}}

其中 intent 必须是以上某一项的英文名(如 policy-match)或 normal_chat。
confidence 是你对该判断的置信度。

用户消息: {}"#,
            sop_list, message
        )
    }
}
