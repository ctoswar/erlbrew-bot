//! Facebook Graph API client (send messages, typing indicators, profile lookups).

use crate::config::Settings;
pub use crate::forms::Reply;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

/// Messenger limits.
const MAX_TEXT: usize = 1900;
const MAX_QUICK_REPLIES: usize = 13;
const MAX_QR_TITLE: usize = 20;

pub struct Messenger {
    settings: Arc<Settings>,
    http: reqwest::Client,
}

/// Split `text` into chunks of at most `max` chars, preferring line breaks.
pub fn chunk_text(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        let mut line = line;
        while line.chars().count() > max {
            let cut = line.char_indices().nth(max).map(|(i, _)| i).unwrap_or(line.len());
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            chunks.push(line[..cut].to_string());
            line = &line[cut..];
        }
        if cur.chars().count() + line.chars().count() > max {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    chunks.into_iter().map(|c| c.trim_end().to_string()).filter(|c| !c.is_empty()).collect()
}

/// Build a Send API body.
pub fn message_body(psid: &str, text: &str, quick_replies: &[(String, String)]) -> Value {
    let mut message = json!({ "text": text });
    if !quick_replies.is_empty() {
        let qrs: Vec<Value> = quick_replies
            .iter()
            .take(MAX_QUICK_REPLIES)
            .map(|(title, payload)| {
                json!({
                    "content_type": "text",
                    "title": title.chars().take(MAX_QR_TITLE).collect::<String>(),
                    "payload": payload,
                })
            })
            .collect();
        message["quick_replies"] = Value::Array(qrs);
    }
    json!({ "recipient": { "id": psid }, "messaging_type": "RESPONSE", "message": message })
}

impl Messenger {
    pub fn new(settings: Arc<Settings>, http: reqwest::Client) -> Self {
        Self { settings, http }
    }

    fn url(&self, path: &str) -> String {
        format!("https://graph.facebook.com/{}/{}", self.settings.graph_api_version, path)
    }

    /// POST to the Graph API. The token goes in a header and URLs are stripped from errors so
    /// it can never leak into logs.
    pub async fn post_graph(&self, path: &str, payload: &Value) -> Result<Value> {
        if self.settings.page_access_token.is_empty() {
            bail!("PAGE_ACCESS_TOKEN is not set");
        }
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.settings.page_access_token)
            .json(payload)
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("graph request failed")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("graph API returned {status}: {body}");
        }
        Ok(body)
    }

    /// Send a reply and return the message ids the Graph API assigned to it, so the caller can
    /// recognise the bot's own echoes later.
    pub async fn send_reply(&self, psid: &str, reply: &Reply) -> Result<Vec<String>> {
        let chunks = chunk_text(&reply.text, MAX_TEXT);
        let last = chunks.len().saturating_sub(1);
        let mut mids = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let qrs: &[(String, String)] = if i == last { &reply.quick_replies } else { &[] };
            let body = self.post_graph("me/messages", &message_body(psid, chunk, qrs)).await?;
            if let Some(mid) = body.get("message_id").and_then(Value::as_str) {
                mids.push(mid.to_string());
            }
        }
        Ok(mids)
    }

    async fn sender_action(&self, psid: &str, action: &str) {
        let body = json!({ "recipient": { "id": psid }, "sender_action": action });
        if let Err(err) = self.post_graph("me/messages", &body).await {
            debug!(error = %err, action, "sender_action failed");
        }
    }

    pub async fn start_typing(&self, psid: &str) {
        self.sender_action(psid, "typing_on").await;
    }

    pub async fn stop_typing(&self, psid: &str) {
        self.sender_action(psid, "typing_off").await;
    }

    /// Best-effort display name for nicer Telegram notifications.
    pub async fn get_profile_name(&self, psid: &str) -> Option<String> {
        if self.settings.page_access_token.is_empty() {
            return None;
        }
        let resp = self
            .http
            .get(self.url(psid))
            .query(&[("fields", "first_name,last_name")])
            .bearer_auth(&self.settings.page_access_token)
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        let name = format!(
            "{} {}",
            v.get("first_name").and_then(Value::as_str).unwrap_or(""),
            v.get("last_name").and_then(Value::as_str).unwrap_or("")
        );
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking() {
        assert_eq!(chunk_text("hello", 10), vec!["hello"]);
        let long = "a".repeat(25);
        assert_eq!(chunk_text(&long, 10).len(), 3);
        let lines = "aaaa\nbbbb\ncccc\n";
        assert_eq!(chunk_text(lines, 10), vec!["aaaa\nbbbb", "cccc"]);
    }

    #[test]
    fn body_has_quick_replies() {
        let qrs = vec![("A very long quick reply title here".to_string(), "P".to_string())];
        let body = message_body("123", "hi", &qrs);
        assert_eq!(body["recipient"]["id"], "123");
        assert_eq!(body["message"]["quick_replies"][0]["title"].as_str().unwrap().chars().count(), 20);
    }
}
