//! Ollama fallback for questions the FAQ cannot answer.

use crate::config::Settings;
use crate::faq::FaqStore;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

pub struct Llm {
    settings: Arc<Settings>,
    http: reqwest::Client,
}

const SYSTEM_PROMPT: &str = "You are the Messenger assistant of Erlbrew Café, a specialty coffee \
and matcha café in Kawit, Cavite, Philippines. Reply in the customer's language (Taglish is fine), \
politely, in at most 3 short sentences. Use ONLY the knowledge below. If the answer is not there, \
say the team will confirm. NEVER confirm prices, availability, payment methods, delivery schedules \
or event dates yourself — the team does that. Never invent menu items.";

impl Llm {
    pub fn new(settings: Arc<Settings>, http: reqwest::Client) -> Self {
        Self { settings, http }
    }

    /// Returns None when the model is unreachable, slow, or answers nothing.
    pub async fn generate(&self, faq: &FaqStore, user_text: &str) -> Option<String> {
        let system = format!("{SYSTEM_PROMPT}\n\n--- KNOWLEDGE ---\n{}", faq.knowledge());
        let body = json!({
            "model": self.settings.ollama_model,
            "stream": false,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user_text },
            ],
            "options": { "temperature": 0.3, "num_predict": 300 },
        });
        let url = format!("{}/api/chat", self.settings.ollama_url.trim_end_matches('/'));
        let resp = match self.http.post(url).json(&body).send().await {
            Ok(r) => r,
            Err(err) => {
                warn!(error = %err.without_url(), "ollama request failed");
                return None;
            }
        };
        if !resp.status().is_success() {
            warn!(status = %resp.status(), "ollama returned an error");
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        let answer = v.pointer("/message/content")?.as_str()?.trim().to_string();
        if answer.is_empty() {
            return None;
        }
        info!(chars = answer.len(), "llm answered");
        Some(answer.chars().take(1900).collect())
    }
}
