//! Background SOP runner — spawned by /chat after intent classification
//! decides a message routes to a SOP. Calls daemon `/api/chat`
//! synchronously (~6-10 min) and updates sop_tasks + emits SSE event.
//!
//! Cache hit doesn't go through here — chat handler short-circuits and
//! calls `sop_tasks::insert_cached_done` directly.

use crate::config::Config;
use crate::provisioner::Provisioner;
use crate::sop_tasks;
use regex::Regex;
use serde_json::Value as JsonValue;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// Inputs needed to run a SOP task in the background. We pass the
/// minimum set of cloned state instead of AppState to keep the
/// dependency surface small.
pub struct SopRunCtx {
    pub pool: SqlitePool,
    pub http: reqwest::Client,
    pub provisioner: Arc<Provisioner>,
    pub cfg: Arc<Config>,
    pub sop_event_tx: broadcast::Sender<JsonValue>,

    pub task_id: String,
    pub openid: String,
    pub sop_name: String,
    pub user_message: String,
}

/// Fire-and-forget: marks task running, calls daemon, updates final
/// state, broadcasts SSE events at each transition.
pub fn spawn(ctx: SopRunCtx) {
    tokio::spawn(async move {
        if let Err(e) = run(&ctx).await {
            tracing::error!(task_id = %ctx.task_id, openid = %ctx.openid, "sop_runner fatal: {e}");
            let _ = sop_tasks::mark_failed(&ctx.pool, &ctx.task_id, &format!("internal: {e}")).await;
            emit(&ctx, "failed");
        }
    });
}

async fn run(ctx: &SopRunCtx) -> anyhow::Result<()> {
    // Step 1: mark running + emit created
    sop_tasks::mark_running(&ctx.pool, &ctx.task_id).await?;
    emit(ctx, "created");

    // Step 2: resolve daemon port for this user
    let user = ctx
        .provisioner
        .ensure_running(&ctx.openid)
        .await
        .map_err(|e| anyhow::anyhow!("ensure_running: {e}"))?;
    let port = user
        .port
        .ok_or_else(|| anyhow::anyhow!("user {} has no port", ctx.openid))?;
    let paired_token = user
        .paired_token_enc
        .ok_or_else(|| anyhow::anyhow!("user {} has no paired token", ctx.openid))?;

    // Step 3: call daemon /api/chat (long, up to ~10 min)
    let url = format!("http://127.0.0.1:{port}/api/chat");
    let resp = ctx
        .http
        .post(&url)
        .timeout(Duration::from_secs(900))
        .header("Authorization", format!("Bearer {paired_token}"))
        .json(&serde_json::json!({"message": ctx.user_message}))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("daemon network error: {e}");
            sop_tasks::mark_failed(&ctx.pool, &ctx.task_id, &user_visible(&msg)).await?;
            emit(ctx, "failed");
            return Ok(());
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let msg = format!("daemon /api/chat returned {}: {}", status, body);
        sop_tasks::mark_failed(&ctx.pool, &ctx.task_id, &user_visible(&msg)).await?;
        emit(ctx, "failed");
        return Ok(());
    }

    let body: JsonValue = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("daemon response JSON parse: {e}");
            sop_tasks::mark_failed(&ctx.pool, &ctx.task_id, &user_visible(&msg)).await?;
            emit(ctx, "failed");
            return Ok(());
        }
    };

    let response_text = body
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    // Extract deeplink + qualification_enterprise_id from response_text.
    // The SOP template instructs the LLM to render a markdown link
    // like `[小程序政策匹配页](/pages/recommendation/index?id=100)`.
    // We accept both this format and bare-path / general /pages/.../?id=N.
    let (deeplink, qid) = extract_deeplink_and_qid(&response_text);

    // enterprise_id isn't parsed from response_text (it's not in the
    // final LLM message). It would require reading the daemon's
    // workspace file (case/policy-match/<safe>/profile.json) — we skip
    // for MVP; only qualification_enterprise_id is needed for deeplink.
    sop_tasks::mark_done(
        &ctx.pool,
        &ctx.task_id,
        None,             // enterprise_id (not extracted, can populate later)
        qid,
        deeplink.as_deref(),
        &response_text,
    )
    .await?;
    emit(ctx, "done");

    Ok(())
}

/// Translate technical errors into something safe to show in
/// task.error_message. Keeps PII/internal stack traces out.
fn user_visible(internal: &str) -> String {
    let lower = internal.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("eof") {
        "服务繁忙,请稍后重试".to_string()
    } else if lower.contains("authentication") || lower.contains("401") {
        "服务配置异常,请联系管理员".to_string()
    } else if lower.contains("enterprise") && lower.contains("not") {
        "未找到您的企业信息,请先补全企业资料".to_string()
    } else {
        // Generic — still better than leaking stack
        "任务执行失败,请稍后重试".to_string()
    }
}

fn extract_deeplink_and_qid(text: &str) -> (Option<String>, Option<i64>) {
    // Capture group 1: the path; group 2: id value
    let re = Regex::new(r"(/pages/[a-zA-Z0-9_/]+\?id=(\d+))").ok();
    let re = match re {
        Some(r) => r,
        None => return (None, None),
    };
    if let Some(caps) = re.captures(text) {
        let path = caps.get(1).map(|m| m.as_str().to_string());
        let id = caps.get(2).and_then(|m| m.as_str().parse::<i64>().ok());
        (path, id)
    } else {
        (None, None)
    }
}

fn emit(ctx: &SopRunCtx, event: &str) {
    let payload = serde_json::json!({
        "type": "sop_task",
        "task_id": ctx.task_id,
        "event": event,
        "openid": ctx.openid,  // stripped by SSE handler before sending to client
    });
    let _ = ctx.sop_event_tx.send(payload);
}
