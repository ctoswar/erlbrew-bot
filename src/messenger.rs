//! Facebook Messenger send side (Graph API).
//!
//! - Uses `Authorization: Bearer <page token>` so tokens never appear in URLs.
//! - Public functions swallow API errors and return false after logging.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::config::Settings;
use crate::db::Db;

const GRAPH_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, thiserror::Error)]
#[error("graph error op={op} status={status} detail={detail}")]
pub struct MessengerError {
    pub op: String,
    pub status: u16,
    pub detail: String,
}

impl MessengerError {
    fn network(op: &str, err: reqwest::Error) -> Self {
        Self {
            op: op.to_string(),
            status: 0,
            detail: format!("network: {err}"),
        }
    }

    fn api(op: &str, status: u16, detail: String) -> Self {
        Self {
            op: op.to_string(),
            status,
            detail,
        }
    }
}

pub struct Messenger {
    cfg: Arc<Settings>,
    db: Db,
    http: reqwest::Client,
    typing: tokio::sync::Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Clone for Messenger {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            db: self.db.clone(), // Db implements Clone via Arc<Mutex<Connection>>
            http: self.http.clone(),
            typing: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl Messenger {
    pub fn new(cfg: Arc<Settings>, db: Db, http: reqwest::Client) -> Self {
        Self {
            cfg,
            db,
            http,
            typing: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// POST to `{GRAPH_BASE}/{version}/{op}`. Returns parsed JSON.
    pub async fn post_graph(&self, op: &str, payload: &Value) -> Result<Value, MessengerError> {
        let url = format!(
            "{}/{}/{}",
            self.cfg.graph_base_url.trim_end_matches('/'),
            self.cfg.graph_version,
            op
        );
        let resp = self
            .http
            .post(&url)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.cfg.page_access_token),
            )
            .header(CONTENT_TYPE, "application/json")
            .json(payload)
            .timeout(GRAPH_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| MessengerError::network(op, e))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status >= 400 || data.get("error").is_some() {
            let detail = data
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(MessengerError::api(op, status, detail));
        }

        // Remember our own message ids: Meta echoes them back to the webhook,
        // and we must not mistake our reply for a human joining the chat.
        if let Some(mid) = data.get("message_id").and_then(Value::as_str) {
            if !mid.is_empty() {
                if let Err(err) = self.db.mark_bot_message(mid).await {
                    tracing::warn!(error = %err, "could not record bot message id");
                }
            }
        }
        Ok(data)
    }

    /// GET `{GRAPH_BASE}/{version}/{path}?fields=...` (e.g. user profile).
    pub async fn get_graph(&self, path: &str, fields: &str) -> Result<Value, MessengerError> {
        let url = format!(
            "{}/{}/{}",
            self.cfg.graph_base_url.trim_end_matches('/'),
            self.cfg.graph_version,
            path
        );
        let resp = self
            .http
            .get(&url)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.cfg.page_access_token),
            )
            .query(&[("fields", fields)])
            .timeout(GRAPH_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| MessengerError::network(path, e))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status >= 400 || data.get("error").is_some() {
            let detail = data
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(MessengerError::api(path, status, detail));
        }
        Ok(data)
    }

    pub async fn send_text(&self, psid: &str, text: &str) -> bool {
        self.send_quick_replies(psid, text, None).await
    }

    pub async fn send_quick_replies(
        &self,
        psid: &str,
        text: &str,
        quick_replies: Option<&[(String, String)]>,
    ) -> bool {
        let payload = message_payload(psid, text, quick_replies);
        match self.post_graph("me/messages", &payload).await {
            Ok(_) => true,
            Err(err) => {
                tracing::error!(op = %err.op, status = err.status, detail = %err.detail, "send failed");
                false
            }
        }
    }

    pub async fn send_typing(&self, psid: &str, action: &str) -> bool {
        let payload = json!({
            "recipient": { "id": psid },
            "sender_action": action,
        });
        match self.post_graph("me/messages", &payload).await {
            Ok(_) => true,
            Err(err) => {
                tracing::error!(op = %err.op, status = err.status, "typing failed");
                false
            }
        }
    }

    /// Best-effort profile fetch; returns None on any error.
    pub async fn get_profile_name(&self, psid: &str) -> Option<String> {
        let data = match self.get_graph(psid, "first_name,last_name").await {
            Ok(data) => data,
            Err(err) => {
                tracing::warn!(status = err.status, detail = %err.detail, "profile fetch failed");
                return None;
            }
        };
        let first = data
            .get("first_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let last = data
            .get("last_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let name = [first, last]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    // --- typing keeper ---------------------------------------------------
    /// Start (or restart) the typing keeper for a PSID. Fire-and-forget.
    pub async fn start_typing(self: &Arc<Self>, psid: &str) {
        self.stop_typing(psid).await;
        let interval = if self.cfg.typing_resend_sec > 0.0 {
            self.cfg.typing_resend_sec
        } else {
            10.0
        };
        let me = self.clone();
        let psid_owned = psid.to_string();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs_f64(interval)).await;
                me.send_typing(&psid_owned, "typing_on").await;
            }
        });
        self.typing
            .lock()
            .await
            .insert(psid.to_string(), handle);
    }

    pub async fn stop_typing(&self, psid: &str) {
        if let Some(handle) = self.typing.lock().await.remove(psid) {
            handle.abort();
        }
    }
}

pub fn message_payload(
    psid: &str,
    text: &str,
    quick_replies: Option<&[(String, String)]>,
) -> Value {
    let text: String = text.chars().take(2000).collect();
    let mut message = json!({ "text": text });
    if let Some(tiles) = quick_replies {
        if !tiles.is_empty() {
            let replies: Vec<Value> = tiles
                .iter()
                .map(|(title, payload)| {
                    json!({
                        "content_type": "text",
                        "title": title.chars().take(20).collect::<String>(),
                        "payload": payload,
                    })
                })
                .collect();
            message["quick_replies"] = Value::Array(replies);
        }
    }
    json!({ "recipient": { "id": psid }, "message": message })
}
