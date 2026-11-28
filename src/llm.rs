//! Ollama fallback for questions the FAQ cannot answer.

use crate::config::Settings;
use crate::faq::FaqStore;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
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

    fn endpoint(&self) -> String {
        format!("{}/api/chat", self.settings.ollama_url.trim_end_matches('/'))
    }

    fn body(&self, system: &str, user_text: &str, num_predict: i64) -> Value {
        json!({
            "model": self.settings.ollama_model,
            "stream": false,
            "think": self.settings.ollama_think,
            "keep_alive": self.settings.ollama_keep_alive,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user_text },
            ],
            "options": {
                "temperature": self.settings.ollama_temperature,
                "num_predict": num_predict,
                "num_ctx": self.settings.ollama_num_ctx,
            },
        })
    }

    /// Load the model and keep it resident so the first customer does not pay the cold start.
    /// Failures are logged only; the next real request will try again.
    pub async fn warm_up(&self) {
        let url = self.endpoint();
        let body = self.body(SYSTEM_PROMPT, "ping", 1);
        for attempt in 1..=3 {
            // Cold model loads can take a minute on CPU; do not let the normal
            // customer-facing timeout abort the warm-up.
            match self
                .http
                .post(&url)
                .timeout(Duration::from_secs(300))
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!("ollama model is warm");
                    return;
                }
                Ok(resp) => warn!(status = %resp.status(), attempt, "ollama warm-up failed"),
                Err(err) => warn!(error = %err.without_url(), attempt, "ollama warm-up failed"),
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    /// Returns None when the model is unreachable, slow, or answers nothing.
    pub async fn generate(&self, faq: &FaqStore, user_text: &str) -> Option<String> {
        let system = format!("{SYSTEM_PROMPT}\n\n--- KNOWLEDGE ---\n{}", faq.knowledge());
        let body = self.body(&system, user_text, self.settings.ollama_num_predict);
        let resp = match self.http.post(self.endpoint()).json(&body).send().await {
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
