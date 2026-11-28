//! Telegram notifications to the café team. Failures are logged, never propagated.

use crate::config::Settings;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, warn};

pub struct Notifier {
    settings: Arc<Settings>,
    http: reqwest::Client,
}

impl Notifier {
    pub fn new(settings: Arc<Settings>, http: reqwest::Client) -> Self {
        Self { settings, http }
    }

    async fn send(&self, text: &str) {
        let (token, chat) = (&self.settings.telegram_bot_token, &self.settings.telegram_chat_id);
        if token.is_empty() || chat.is_empty() {
            debug!("telegram not configured; skipping notification");
            return;
        }
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = json!({ "chat_id": chat, "text": text, "disable_web_page_preview": true });
        match self.http.post(url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => warn!(status = %resp.status(), "telegram rejected notification"),
            // without_url(): the URL contains the bot token.
            Err(err) => warn!(error = %err.without_url(), "telegram notification failed"),
        }
    }

    pub async fn notify_handoff(&self, psid: &str, name: &str, text: &str) {
        let who = if name.is_empty() { "(unknown name)" } else { name };
        self.send(&format!(
            "🙋 Customer needs a human\nFrom: {who}\nPSID: {psid}\nMessage: {text}\n\n\
             The bot is paused for this customer. Resume with:\nPOST /admin/unpause/{psid}"
        ))
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn notify_event_lead(
        &self,
        psid: &str,
        lead_id: i64,
        event_type: &str,
        when: &str,
        guests: i64,
        name: &str,
        contact: &str,
    ) {
        self.send(&format!(
            "🎉 New event lead #{lead_id}\nType: {event_type}\nWhen: {when}\nGuests: {guests}\n\
             Name: {name}\nContact: {contact}\nPSID: {psid}\n\n\
             Reply in Messenger to confirm availability and price."
        ))
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn notify_order(
        &self,
        psid: &str,
        order_id: i64,
        items: &str,
        total_estimate: &str,
        name: &str,
        address: &str,
        contact: &str,
    ) {
        let total = if total_estimate.is_empty() {
            "(no price estimate)".to_string()
        } else {
            format!("₱{total_estimate} (ESTIMATE)")
        };
        self.send(&format!(
            "🛍️ New order #{order_id}\n{items}\nTotal: {total}\nName: {name}\nAddress: {address}\n\
             Contact: {contact}\nPSID: {psid}\n\nConfirm total, payment and delivery in Messenger."
        ))
        .await;
    }
}
