//! Message pipeline — the order of checks:
//!   1. Ignore echoes (our own messages and staff replies typed in the page inbox)
//!   2. Dedupe by message id
//!   3. Per-user rate limit (15 / 10 min)
//!   4. 500-char text cap
//!   5. Paused users get no replies
//!   6. Quick-reply / postback payloads
//!   7. Acknowledgements / closings ("thank you", "ok", 👍) -> one warm line
//!   8. Active event/order form continues
//!   9. Handoff keywords -> pause + Telegram
//!  10. Order / event intent starts a form
//!  11. Keyword/regex FAQ match
//!  12. LLM fallback

use crate::acks;
use crate::config::{
    EVENT_INTENT_RE, HANDOFF_RE, HANDOFF_TEXT, HELP_TEXT, LLM_FALLBACK_TEXT, NON_TEXT_TEXT,
    ORDER_INTENT_RE, RATE_LIMITED_TEXT, TOO_LONG_TEXT,
};
use crate::forms::{self, FormKind, Reply};
use crate::state::App;
use crate::{events, orders};
use anyhow::Result;

/// A normalised customer event (message, quick reply or postback).
#[derive(Clone, Debug, Default)]
pub struct Incoming {
    pub psid: String,
    pub mid: String,
    pub text: String,
    pub payload: String,
    pub is_echo: bool,
}

pub async fn process(app: &App, inc: Incoming) -> Result<()> {
    let psid = inc.psid.as_str();

    // 1) Echoes: our own messages are ignored; so are staff replies typed in the page inbox.
    if inc.is_echo {
        app.db.upsert_user(psid)?;
        return Ok(());
    }

    // 2) Dedupe by message id (Meta redelivers).
    if !inc.mid.is_empty() && !app.db.mark_message_seen(&inc.mid)? {
        return Ok(());
    }

    // 3) Rate limit: one polite notice, then ignore.
    let window = app.settings.rate_limit_window_sec;
    app.db.note_rate_event(psid)?;
    if app.db.count_rate_events(psid, window)? > app.settings.rate_limit_max {
        if app.db.pop_notice_once(psid, "rate", window)? {
            app.send(psid, &Reply::text(RATE_LIMITED_TEXT)).await;
        }
        return Ok(());
    }

    // 4) Max text length: one polite notice, then ignore.
    if inc.text.chars().count() > app.settings.max_text_chars {
        if app.db.pop_notice_once(psid, "toolong", 600)? {
            app.send(psid, &Reply::text(TOO_LONG_TEXT)).await;
        }
        return Ok(());
    }

    // 5) Paused users get no replies.
    app.db.upsert_user(psid)?;
    if app.db.get_paused(psid)? {
        return Ok(());
    }

    // Best-effort profile name for nicer Telegram notifications.
    if app.db.get_user_name(psid)?.is_empty() {
        if let Some(name) = app.messenger.get_profile_name(psid).await {
            let _ = app.db.set_user_name(psid, &name);
        }
    }

    app.messenger.start_typing(psid).await;
    let result = respond(app, &inc).await;
    app.messenger.stop_typing(psid).await;
    result
}

async fn respond(app: &App, inc: &Incoming) -> Result<()> {
    let psid = inc.psid.as_str();
    let text = inc.text.trim();

    // 6) Payloads (quick replies, menu buttons, postbacks).
    if !inc.payload.trim().is_empty() && route_payload(app, psid, &inc.payload).await {
        return Ok(());
    }

    // Non-text (stickers, images): re-ask the current form question, or ask for text.
    if text.is_empty() {
        let reply = current_form_question(app, psid).unwrap_or_else(|| Reply::text(NON_TEXT_TEXT));
        app.send(psid, &reply).await;
        return Ok(());
    }

    // 7) Acknowledgements / closings ("thank you", "ok", "sige", 👍).
    // One warm line, never the welcome message and never another "How can I help?".
    if let Some(ack) = acks::detect(text) {
        // While the receipt waits for confirmation, "ok"/"sige" means YES — the order form
        // handles it instead of closing the conversation.
        let confirm_pending = app
            .db
            .get_form(psid)
            .ok()
            .flatten()
            .is_some_and(|row| row.step == "order_confirm");
        if !(confirm_pending && orders::is_confirm_yes(text)) {
            app.send(psid, &acks::reply(app, psid, ack)).await;
            return Ok(());
        }
    }

    // 8) An active form continues (form before handoff keywords).
    if let Some(reply) = continue_form(app, psid, text).await {
        app.send(psid, &reply).await;
        return Ok(());
    }

    // 9) Handoff keywords -> short message, pause, Telegram.
    if HANDOFF_RE.is_match(text) {
        app.db.set_paused(psid, true)?;
        let name = app.db.get_user_name(psid).unwrap_or_default();
        let notifier = app.notifier.clone();
        let (p, t) = (psid.to_string(), text.to_string());
        tokio::spawn(async move { notifier.notify_handoff(&p, &name, &t).await });
        app.send(psid, &Reply::text(HANDOFF_TEXT)).await;
        return Ok(());
    }

    // 10) Intent -> start a form (AFTER handoff, so "bulk orders" still reaches the team).
    if ORDER_INTENT_RE.is_match(text) {
        app.send(psid, &orders::start_form(app, psid)).await;
        return Ok(());
    }
    if EVENT_INTENT_RE.is_match(text) {
        app.send(psid, &events::start_form(app, psid)).await;
        return Ok(());
    }

    // 11) Keyword/regex FAQ.
    if let Some(entry) = app.faq.match_text(text) {
        if !entry.answer.is_empty() {
            app.send(psid, &Reply::new(entry.answer.clone(), entry.quick_reply_pairs())).await;
            return Ok(());
        }
    }

    // 12) LLM fallback.
    if let Some(llm) = &app.llm {
        if let Some(answer) = llm.generate(&app.faq, text).await {
            app.send(psid, &Reply::text(answer)).await;
            return Ok(());
        }
    }
    app.send(psid, &Reply::text(LLM_FALLBACK_TEXT)).await;
    Ok(())
}

async fn continue_form(app: &App, psid: &str, text: &str) -> Option<Reply> {
    match forms::active_kind(&app.db, psid)? {
        FormKind::Order => orders::handle_input(app, psid, text).await,
        FormKind::Event => events::handle_input(app, psid, text).await,
    }
}

fn current_form_question(app: &App, psid: &str) -> Option<Reply> {
    let kind = forms::active_kind(&app.db, psid)?;
    let row = app.db.get_form(psid).ok().flatten()?;
    Some(match kind {
        FormKind::Order => orders::current_question(&row),
        FormKind::Event => events::current_question(&row),
    })
}

/// Route a quick-reply / postback payload. Always handles it (unknown payloads get the help
/// text), so it returns true once a reply has been sent.
pub async fn route_payload(app: &App, psid: &str, payload: &str) -> bool {
    let p = payload.trim().to_uppercase();

    let reply: Option<Reply> = if p == "ORDER_START" {
        Some(orders::start_form(app, psid))
    } else if p == "EVT_START" {
        match (forms::active_kind(&app.db, psid), current_form_question(app, psid)) {
            (Some(FormKind::Event), Some(question)) => Some(question),
            _ => Some(events::start_form(app, psid)),
        }
    } else if p == orders::PAYLOAD_OK || p == orders::PAYLOAD_EDIT {
        // Only meaningful while an order form is waiting for confirmation.
        orders::handle_input(app, psid, &p).await
    } else if p.starts_with(events::TYPE_PAYLOAD_PREFIX) {
        if forms::active_kind(&app.db, psid) != Some(FormKind::Event) {
            events::start_form(app, psid);
        }
        events::handle_input(app, psid, &p).await
    } else {
        None
    };
    if let Some(reply) = reply {
        app.send(psid, &reply).await;
        return true;
    }

    if let Some(entry) = app.faq.match_payload(&p) {
        if !entry.answer.is_empty() {
            app.send(psid, &Reply::new(entry.answer.clone(), entry.quick_reply_pairs())).await;
            return true;
        }
    }

    // Unknown / stale payload -> friendly reset.
    let help = app
        .faq
        .by_id("fallback_help")
        .map(|e| Reply::new(e.answer.clone(), e.quick_reply_pairs()))
        .unwrap_or_else(|| Reply::text(HELP_TEXT));
    app.send(psid, &help).await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(psid: &str, mid: &str, text: &str) -> Incoming {
        Incoming { psid: psid.into(), mid: mid.into(), text: text.into(), ..Default::default() }
    }

    fn step(app: &App, psid: &str) -> Option<String> {
        app.db.get_form(psid).unwrap().map(|r| r.step)
    }

    #[tokio::test]
    async fn full_order_flow() {
        let app = App::for_test();
        let u = "user1";
        for (i, (text, expect_step)) in [
            ("gusto ko mag order", Some("order_items")),
            ("2 iced latte + 1 cookie", Some("order_confirm")),
            ("tama po", Some("order_name")),
            ("Maria Santos", Some("order_address")),
            ("Blk 2 Lot 3 Masaito Residence Kawit", Some("order_contact")),
            ("0917 123 4567", None),
        ]
        .into_iter()
        .enumerate()
        {
            process(&app, msg(u, &format!("m{i}"), text)).await.unwrap();
            assert_eq!(step(&app, u).as_deref(), expect_step, "after '{text}'");
        }
        let orders = app.db.list_orders(5).unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0]["name"], "Maria Santos", "name must keep its original casing");
        assert_eq!(orders[0]["contact"], "09171234567");
    }

    #[tokio::test]
    async fn edit_cancel_and_gave_up() {
        let app = App::for_test();
        let u = "user2";
        process(&app, msg(u, "a1", "order")).await.unwrap();
        process(&app, msg(u, "a2", "1 latte")).await.unwrap();
        process(&app, msg(u, "a3", "mali")).await.unwrap();
        assert_eq!(step(&app, u).as_deref(), Some("order_items"));
        process(&app, msg(u, "a4", "cancel po")).await.unwrap();
        assert_eq!(step(&app, u), None);

        // Three bad answers in a row abandon the form.
        process(&app, msg(u, "b1", "order")).await.unwrap();
        for i in 0..3 {
            process(&app, msg(u, &format!("b-x{i}"), "x")).await.unwrap();
        }
        assert_eq!(step(&app, u), None);
    }

    #[tokio::test]
    async fn event_flow_via_payloads() {
        let app = App::for_test();
        let u = "user3";
        process(&app, Incoming { psid: u.into(), mid: "e1".into(), payload: "EVT_START".into(), ..Default::default() }).await.unwrap();
        assert_eq!(step(&app, u).as_deref(), Some("event_type"));
        process(&app, Incoming { psid: u.into(), mid: "e2".into(), text: "Birthday 🎂".into(), payload: "EVT_TYPE:BIRTHDAY".into(), ..Default::default() }).await.unwrap();
        assert_eq!(step(&app, u).as_deref(), Some("event_when"));
        for (i, text) in ["May 12, 3 PM", "12", "Ana Cruz", "09171234567"].into_iter().enumerate() {
            process(&app, msg(u, &format!("e-{i}"), text)).await.unwrap();
        }
        assert_eq!(step(&app, u), None);
    }

    #[tokio::test]
    async fn handoff_pauses_and_dedupes() {
        let app = App::for_test();
        let u = "user4";
        process(&app, msg(u, "h1", "I want a refund")).await.unwrap();
        assert!(app.db.get_paused(u).unwrap());

        // Paused: orders are ignored.
        process(&app, msg(u, "h2", "order")).await.unwrap();
        assert_eq!(step(&app, u), None);

        // Duplicate delivery is ignored.
        let v = "user5";
        process(&app, msg(v, "dup", "order")).await.unwrap();
        process(&app, msg(v, "dup", "cancel")).await.unwrap();
        assert_eq!(step(&app, v).as_deref(), Some("order_items"));
    }

    #[tokio::test]
    async fn bulk_orders_go_to_team_not_form() {
        let app = App::for_test();
        process(&app, msg("user6", "k1", "bulk orders po for office")).await.unwrap();
        assert!(app.db.get_paused("user6").unwrap());
        assert_eq!(step(&app, "user6"), None);
    }

    #[tokio::test]
    async fn acknowledgement_closes_warmly_and_keeps_forms_open() {
        let app = App::for_test();
        let u = "ack-user";

        // A plain closing does not start a form and does not pause the customer.
        process(&app, msg(u, "a1", "salamat po")).await.unwrap();
        assert_eq!(step(&app, u), None);
        assert!(!app.db.get_paused(u).unwrap());

        // An acknowledgement inside a form keeps the form open (no "gave up" counter).
        process(&app, msg(u, "a2", "order")).await.unwrap();
        process(&app, msg(u, "a3", "ok")).await.unwrap();
        assert_eq!(step(&app, u).as_deref(), Some("order_items"));

        // "ok" while the receipt waits still means YES.
        process(&app, msg(u, "a4", "2 iced latte")).await.unwrap();
        assert_eq!(step(&app, u).as_deref(), Some("order_confirm"));
        process(&app, msg(u, "a5", "sige")).await.unwrap();
        assert_eq!(step(&app, u).as_deref(), Some("order_name"));
    }

    #[tokio::test]
    async fn rate_limit_kicks_in() {
        let app = App::for_test();
        for i in 0..20 {
            process(&app, msg("spam", &format!("s{i}"), "hello")).await.unwrap();
        }
        assert!(app.db.count_rate_events("spam", 600).unwrap() > app.settings.rate_limit_max);
    }
}
