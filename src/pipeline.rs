//! Message pipeline processing — the same order of checks as the Python bot:
//!   1. Ignore bot's own echoes
//!   2. Dedupe by message id
//!   3. Per-user rate limit (15/10 min)
//!   4. 500-char text cap
//!   5. Paused users get no replies
//!   6. Event form continues (if active)
//!   7. Handoff keywords -> pause + Telegram
//!   8. Order intent starts form
//!   9. Keyword/regex FAQ match
//! 10. LLM fallback

use crate::config::Settings;
use crate::db::Db;
use crate::events::handle_input as events_handle_input;
use crate::orders::handle_input as orders_handle_input;
use crate::faq::{FaqStore, text_entry, payload_entry};
use crate::llm::generate;
use crate::messenger::{message_payload, Messenger};
use crate::notify;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Process a single webhook event message. Returns some reply text or None.
pub async fn process_message(
    psid: &str,
    event: &Value,
    db: &Db,
    messenger: &Arc<Messenger>,
    faq: &FaqStore,
    llm_client: &mut Option<reqwest::Client>,
) -> Result<Option<String>, anyhow::Error> {
    let message = event.get("message").and_then(|m| m.as_object());
    let quick_payload = event
        .get("message")
        .and_then(|m| m.get("quick_reply"))
        .and_then(|q| q.get("payload"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let is_echo = event
        .get("message")
        .and_then(|m| m.get("is_echo"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 1) Ignore bot's own echoes
    if is_echo {
        let mid = event
            .get("message")
            .and_then(|m| m.get("mid"))
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !mid.is_empty() && db.is_bot_message(&mid).await? {
            return Ok(None);
        }
        // Still upsert user so we track pauses
        db.upsert_user(psid).await?;
    }

    // 2) Dedupe by message id
    let mid = event
        .get("message")
        .and_then(|m| m.get("mid"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !mid.is_empty() {
        let already_seen = db.mark_message_seen(&mid).await?;
        if !already_seen {
            // Duplicate delivery, silently return (no reply)
            return Ok(None);
        }
    }

    // 3) Rate limit (15 msgs / 10 min) — one polite notice, then ignore
    db.note_rate_event(psid).await?;
    let over = db.count_rate_events(psid, Settings::for_test().rate_limit_window_sec).await? > 15;
    if over {
        let once = db.pop_notice_once(psid, "rate", 600).await?;
        if once {
            // send rate limited text
            return Ok(Some(
                crate::config::RATE_LIMITED_TEXT.to_string(),
            ));
        }
        return Ok(None); // silently ignore
    }

    // 4) Max text length (500 chars) — one polite notice, then ignore
    let text = event
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.len() > Settings::for_test().max_text_chars {
        let once = db.pop_notice_once(psid, "toolong", 600).await?;
        if once {
            return Ok(Some(
                crate::config::TOO_LONG_TEXT.to_string(),
            ));
        }
        return Ok(None); // silently ignore
    }

    // 5) Paused users get no replies
    db.upsert_user(psid).await?;
    if db.get_paused(psid).await? {
        return Ok(None); // silently ignore — bot is paused
    }

    // Best-effort profile name for nicer Telegram notifications
    if db.get_user_name(psid).await?.is_empty() {
        if let Some(name) = crate::messenger::get_profile_name(messenger, psid).await {
            let _ = db.set_user_name(psid, &name).await;
        }
    }

    // Send typing_on before processing
    messenger.start_typing(psid).await;

    // 6) Payloads (quick replies; postbacks handled separately upstream)
    if !quick_payload.is_empty() {
        if crate::routes::route_payload(messenger, db, faq, psid, &quick_payload).await? {
            messenger.stop_typing(psid).await;
            return Ok(None);
        }
    }

    // Non-text messages (images/stickers): ask the customer to type instead.
    if text.is_empty() {
        messenger.stop_typing(psid).await;
        // If there's an active form, re-ask the current question
        if crate::events::has_active_form(db, psid).await {
            if let Some(reply) = crate::events::current_question(db, psid).await {
                let _ = messenger
                    .send_text(psid, &reply.text)
                    .await;
                return Ok(Some(reply.text));
            }
        }
        return Ok(Some(
            crate::config::NON_TEXT_TEXT.to_string(),
        ));
    }

    // 7) Active event form continues (form before handoff keywords)
    if crate::events::has_active_form(db, psid).await {
        if let Some(reply) = events_handle_input(messenger, db, psid, &text).await {
            messenger.stop_typing(psid).await;
            // Send the reply text
            if !reply.text.is_empty() {
                let _ = messenger.send_text(psid, &reply.text).await;
            }
            return Ok(Some(reply.text));
        }
        // Form was deleted (e.g. cancelled)
    }

    // 8) Handoff keywords -> short message, pause user, Telegram notification
    if crate::config::HANDOFF_RE.is_match(&text) || false // placeholder
    {
        // Actually use the compiled regex from main.rs
        // For now: check handoff keywords
        let handoff_keywords = [
            "complaints", "complains", "reklamos", "refunds", "allergies", "allergic",
            "bulk orders", "human", "staff", "tao",
        ];
        let text_lower = text.to_lowercase();
        let matched = handoff_keywords.iter().any(|&kw| text_lower.contains(kw));
        if matched {
            db.set_paused(psid, true).await?;
            let name = db.get_user_name(psid).await.unwrap_or_default();
            let _ = notify::notify_handoff(messenger, db, psid, &name, &text).await;
            messenger.stop_typing(psid).await;
            return Ok(Some(crate::config::HANDOFF_TEXT.to_string()));
        }
    }

    // 8b) Order intent starts the order intake form (AFTER handoff)
    // Check that "bulk order" complaints still go to team
    let order_intent_keywords = ["orders", "pabili", "umorder", "pa-order", "pa-deliver"];
    let text_lower = text.to_lowercase();
    let is_order_intent = order_intent_keywords
        .iter()
        .any(|&kw| text_lower.contains(kw))
        && !handoff_keywords.iter().any(|&kw| text_lower.contains(kw));
    if is_order_intent {
        // Delete any active form first
        let _ = db.delete_form(psid).await;
        if let Some(reply) = orders_handle_input(messenger, db, /* menu */ &[], psid, &text).await {
            messenger.stop_typing(psid).await;
            if !reply.text.is_empty() {
                let _ = messenger.send_text(psid, &reply.text).await;
            }
            return Ok(Some(reply.text));
        }
        return Ok(None);
    }

    // 9) Keyword/regex FAQ match
    if let Some(entry) = text_entry(&text) {
        let answer = entry.answer.clone();
        // Quick replies if any
        if !entry.quick_replies.is_empty() {
            let qrs: Vec<(String, String)> = entry
                .quick_replies
                .iter()
                .map(|qr| (qr.title.clone(), qr.payload.clone()))
                .collect();
            let payload = message_payload(psid, &answer, Some(&qrs));
            messenger.post_graph("me/messages", &payload).await?;
            messenger.stop_typing(psid).await;
            return Ok(Some(answer));
        }
        messenger.stop_typing(psid).await;
        return Ok(Some(answer));
    }

    // 10) LLM fallback
    if let Some(client) = llm_client.as_mut() {
        if let Some(answer) = generate(psid, &text, client, None).await? {
            messenger.stop_typing(psid).await;
            return Ok(Some(answer));
        }
    }

    // Should not reach here, but send fallback just in case
    messenger.stop_typing(psid).await;
    Ok(Some(crate::config::LLM_FALLBACK_TEXT.to_string()))
}

/// Route a postback / quick-reply payload through the appropriate handler.
pub async fn route_payload(
    messenger: &Arc<Messenger>,
    db: &Db,
    faq: &FaqStore,
    psid: &str,
    payload: &str,
) -> bool {
    let p = payload.trim().to_uppercase();

    // ORDER_START payload
    if p == "ORDER_START" {
        if let Some(reply) = orders::start_form(db, psid).await {
            let payload = message_payload(psid, &reply.text, Some(&reply.quick_replies));
            let _ = messenger.post_graph("me/messages", &payload).await;
            return true;
        }
        return false;
    }

    // EVT_START payload
    if p == "EVT_START" {
        if crate::events::has_active_form(db, psid).await {
            if let Some(reply) = crate::events::current_question(db, psid).await {
                let payload = message_payload(psid, &reply.text, Some(&reply.quick_replies));
                let _ = messenger.post_graph("me/messages", &payload).await;
                return true;
            }
        } else {
            if let Some(reply) = crate::events::start_form(db, psid).await {
                let payload = message_payload(psid, &reply.text, Some(&reply.quick_replies));
                let _ = messenger.post_graph("me/messages", &payload).await;
                return true;
            }
        }
        return false;
    }

    // EVT_TYPE: payloads
    if let Some(stripped) = p.strip_prefix("EVT_TYPE:") {
        // This goes into the events form chain
        // For simplicity, just mark the form started with the type
        // The actual step progression happens via handle_input
        let _ = db.set_form(psid, "event_type", &HashMap::from([("type".into(), stripped.into())]), 0).await;
        if let Some(reply) = crate::events::current_question(db, psid).await {
            let payload = message_payload(psid, &reply.text, Some(&reply.quick_replies));
            let _ = messenger.post_graph("me/messages", &payload).await;
            return true;
        }
        return false;
    }

    // Known FAQ payloads
    if let Some(entry) = faq.payload_entry(&p) {
        let answer = entry.answer.clone();
        let payload = message_payload(psid, &answer, Some(&entry.quick_replies.iter().map(|qr| (qr.title.clone(), qr.payload.clone())).collect::<Vec<_>>()));
        let _ = messenger.post_graph("me/messages", &payload).await;
        return true;
    }

    // Unknown payload -> friendly reset
    if let Some(help_entry) = faq.entry_by_id("fallback_help") {
        let _ = messenger
            .send_text(psid, &help_entry.answer)
            .await;
        return true;
    }

    false
}

// Regex for handoff keywords - defined at module level for reuse
static HANDOFF_RE: regex::Regex = regex::Regex::new(
    r"(?i)\b(complaints?|complains?|reklamos?|refunds?|allerg(?:y|ies)|allergic|bulk\s+orders?|human|staff|tao)\b"
)
.expect("handoff regex compile");