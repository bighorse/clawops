use crate::config::QualificationConfig;
use crate::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct QualificationReminder {
    pub id: i64,
    pub openid: String,
    pub enterprise_name: String,
    pub qualification_enterprise_id: Option<i64>,
    pub reminder_type: String,
    pub qualification_title: String,
    pub reg_no: Option<String>,
    pub remind_at: String,
    pub event_date: String,
    pub sent_at: Option<String>,
    pub failed_at: Option<String>,
    pub fail_reason: Option<String>,
    pub created_at: String,
}

/// Single reminder item inside the POST /internal/qualification-reminders body.
#[derive(Debug, Deserialize)]
pub struct ReminderItem {
    pub reminder_type: String,
    pub qualification_title: String,
    #[serde(default)]
    pub reg_no: Option<String>,
    pub remind_at: String,
    pub event_date: String,
}

/// POST /internal/qualification-reminders body.
/// zeroclaw SOP POSTs this after completing the qualification-check flow.
#[derive(Debug, Deserialize)]
pub struct CreateRemindersReq {
    pub openid: String,
    pub enterprise_name: String,
    #[serde(default)]
    pub qualification_enterprise_id: Option<i64>,
    pub reminders: Vec<ReminderItem>,
}

/// Delete pending reminders for (openid, enterprise_name) before
/// inserting a fresh batch. Prevents duplicates on re-run.
pub async fn clear_pending(pool: &SqlitePool, openid: &str, enterprise_name: &str) -> Result<()> {
    sqlx::query(
        "DELETE FROM qualification_reminders \
         WHERE openid = ? AND enterprise_name = ? \
         AND sent_at IS NULL AND failed_at IS NULL",
    )
    .bind(openid)
    .bind(enterprise_name)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_batch(pool: &SqlitePool, req: &CreateRemindersReq) -> Result<usize> {
    let mut count = 0usize;
    for item in &req.reminders {
        sqlx::query(
            "INSERT INTO qualification_reminders \
             (openid, enterprise_name, qualification_enterprise_id, \
              reminder_type, qualification_title, reg_no, remind_at, event_date) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&req.openid)
        .bind(&req.enterprise_name)
        .bind(req.qualification_enterprise_id)
        .bind(&item.reminder_type)
        .bind(&item.qualification_title)
        .bind(&item.reg_no)
        .bind(&item.remind_at)
        .bind(&item.event_date)
        .execute(pool)
        .await?;
        count += 1;
    }
    Ok(count)
}

pub async fn fetch_due(pool: &SqlitePool) -> Result<Vec<QualificationReminder>> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query_as::<_, QualificationReminder>(
        "SELECT * FROM qualification_reminders \
         WHERE sent_at IS NULL AND failed_at IS NULL AND remind_at <= ? \
         ORDER BY remind_at ASC LIMIT 50",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn mark_sent(pool: &SqlitePool, id: i64) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE qualification_reminders SET sent_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_failed(pool: &SqlitePool, id: i64, reason: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE qualification_reminders SET failed_at = ?, fail_reason = ? WHERE id = ?",
    )
    .bind(now)
    .bind(reason)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_for_user(pool: &SqlitePool, openid: &str) -> Result<Vec<QualificationReminder>> {
    let rows = sqlx::query_as::<_, QualificationReminder>(
        "SELECT * FROM qualification_reminders \
         WHERE openid = ? ORDER BY remind_at DESC LIMIT 100",
    )
    .bind(openid)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Build a human-readable reminder message from a QualificationReminder row.
fn build_message(r: &QualificationReminder) -> String {
    let type_cn = match r.reminder_type.as_str() {
        "PATENT_FEE" => "专利年费",
        "PATENT_ANNUAL" => "专利年报",
        "TRADEMARK_RENEWAL" => "商标续展",
        "CERT_RENEWAL" => "资质证书续期",
        "HONOR_RENEWAL" => "荣誉资质复审",
        _ => "资质到期",
    };
    match &r.reg_no {
        Some(no) => format!(
            "【资质提醒】{}：{} ({})，事件日期 {}，请及时办理。",
            type_cn, r.qualification_title, no, r.event_date
        ),
        None => format!(
            "【资质提醒】{}：{}，事件日期 {}，请及时办理。",
            type_cn, r.qualification_title, r.event_date
        ),
    }
}

/// Send SMS via configured sms_send_url (generic JSON POST).
/// Body: {"to": "<phone>", "content": "<text>"}
/// Auth: Bearer sms_api_key (if set).
pub async fn send_sms(
    http: &reqwest::Client,
    cfg: &QualificationConfig,
    phone: &str,
    content: &str,
) -> Result<()> {
    if cfg.sms_send_url.is_empty() {
        return Err(crate::Error::Other("sms_send_url not configured".into()));
    }
    let body = serde_json::json!({"to": phone, "content": content});
    let mut req = http.post(&cfg.sms_send_url).json(&body);
    if !cfg.sms_api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.sms_api_key));
    }
    req.send().await?.error_for_status()?;
    Ok(())
}

/// Send Feishu/Lark bot webhook notification (for uin: wecom users).
pub async fn send_webhook(http: &reqwest::Client, webhook_url: &str, content: &str) -> Result<()> {
    if webhook_url.is_empty() {
        return Err(crate::Error::Other("wecom_webhook_url not configured".into()));
    }
    let body = serde_json::json!({
        "msg_type": "text",
        "content": {"text": content}
    });
    http.post(webhook_url).json(&body).send().await?.error_for_status()?;
    Ok(())
}

/// Try to dispatch a single reminder.
/// Returns Ok(true) if sent, Ok(false) if skipped (no phone / url).
pub async fn dispatch(
    pool: &SqlitePool,
    http: &reqwest::Client,
    cfg: &QualificationConfig,
    r: &QualificationReminder,
) -> std::result::Result<(), String> {
    let msg = build_message(r);
    if r.openid.starts_with("uin:") {
        // WeChat/wecom user — send via webhook
        send_webhook(http, &cfg.wecom_webhook_url, &msg)
            .await
            .map_err(|e| e.to_string())?;
    } else {
        // Mini-program user — look up phone from users table
        let phone: Option<String> =
            sqlx::query_scalar("SELECT phone FROM users WHERE openid = ?")
                .bind(&r.openid)
                .fetch_optional(pool)
                .await
                .unwrap_or(None)
                .flatten();
        match phone {
            Some(p) if !p.is_empty() => {
                send_sms(http, cfg, &p, &msg)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            _ => {
                return Err("no phone number for user".into());
            }
        }
    }
    Ok(())
}
