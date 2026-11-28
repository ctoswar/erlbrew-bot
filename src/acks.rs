//! Acknowledgement & closing messages ("thank you", "ok", "sige", 👍 ...).
//!
//! Short acknowledgements used to fall through to the FAQ / LLM fallback, which answered with
//! a cold "Pasensya po, hindi ko po sigurado…" or repeated the welcome message. They now get
//! ONE short, warm Taglish line (no repeated intro, no "How can I help you today?"), and when a
//! booking/order form is still open the line also carries a gentle reminder of the next step.

use crate::forms::FormKind;
use crate::state::App;
use crate::{events, orders};
use std::sync::atomic::Ordering;

/// What kind of acknowledgement the customer sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    /// "thank you", "salamat", "ty", …
    Thanks,
    /// "ok", "okay", "sige", "noted", "got it", …
    Ok,
}

const THANKS_WORDS: &[&str] = &[
    "thank you",
    "thank you po",
    "thank you so much",
    "thanks",
    "thanks po",
    "thanks a lot",
    "thank u",
    "thx",
    "salamat",
    "salamat po",
    "salamat na",
    "salamat sa lahat",
    "ty",
    "ty po",
];

const OK_WORDS: &[&str] = &[
    "ok",
    "ok po",
    "ok na",
    "okay",
    "okay po",
    "okay na",
    "sige",
    "sige po",
    "sige na",
    "noted",
    "noted po",
    "noted na",
    "got it",
    "gotit",
    "alright",
    "all right",
    "alright po",
    "opo",
    "opo po",
    "yes",
    "yes po",
    "k",
    "kk",
    "cool",
    "nice",
    "perfect",
    "great",
];

/// Closing lines for "thank you" / "salamat" — one line, at most one emoji.
const THANKS_LINES: &[&str] = &[
    "Walang anuman po! ☕ See you sa Erlbrew!",
    "Salamat din po! Sana makita ka namin soon 😊",
    "Welcome po! Message lang po kayo kung may iba pa kayong tanong.",
    "Noted po! Salamat sa pag-message sa Erlbrew 🙏",
    "Salamat po sa pag-message! Ingat po kayo 😊",
    "Salamat po! Sana makita-kayo ulit sa Erlbrew ☕",
];

/// Closing lines for "ok" / "sige" / "noted" — one line, at most one emoji.
const OK_LINES: &[&str] = &[
    "Sige po! Nandito lang kami kung may kailangan pa kayo 😊",
    "Okay po! Ingat po kayo ☕",
    "Noted po! Salamat po sa pakikinig ☕",
    "Sige po! Nandito po kami kung may tanong pa kayo 😊",
];

/// Lowercase, drop punctuation and emoji, collapse whitespace.
fn normalize(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A character that is a symbol/emoji rather than text (👍, 🙏, ☕, …).
fn is_symbol(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !c.is_ascii_punctuation()
}

/// True when the message is only emoji/symbols (👍, 🙏, ❤️) — an acknowledgment too.
fn only_symbols(raw: &str) -> bool {
    raw.chars().any(is_symbol)
}

/// Detect a short acknowledgment / closing message.
pub fn detect(raw: &str) -> Option<Ack> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 60 {
        return None;
    }
    let norm = normalize(trimmed);
    if norm.is_empty() {
        return only_symbols(trimmed).then_some(Ack::Ok);
    }
    if THANKS_WORDS.contains(&norm.as_str()) {
        return Some(Ack::Thanks);
    }
    if OK_WORDS.contains(&norm.as_str()) {
        return Some(Ack::Ok);
    }
    None
}

/// The reply text: one warm line, plus a gentle reminder when a form is still open.
pub fn reply_text(app: &App, psid: &str, ack: Ack) -> String {
    if let Some(kind) = crate::forms::active_kind(&app.db, psid) {
        if let Some(row) = app.db.get_form(psid).ok().flatten() {
            let hint = match kind {
                FormKind::Order => orders::step_hint(&row.step),
                FormKind::Event => events::step_hint(&row.step),
            };
            let lead = match ack {
                Ack::Thanks => "Salamat po!",
                Ack::Ok => "Sige po!",
            };
            return format!("{lead} {hint}");
        }
    }

    let n = app.ack_counter.fetch_add(1, Ordering::Relaxed);
    let pool = match ack {
        Ack::Thanks => THANKS_LINES,
        Ack::Ok => OK_LINES,
    };
    pool[n % pool.len()].to_string()
}

/// The reply to send for an acknowledgement.
pub fn reply(app: &App, psid: &str, ack: Ack) -> crate::forms::Reply {
    crate::forms::Reply::text(reply_text(app, psid, ack))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::App;
    use std::collections::HashMap;

    fn symbols(text: &str) -> usize {
        text.chars().filter(|c| is_symbol(*c)).count()
    }

    #[test]
    fn detects_acknowledgements() {
        for text in [
            "thank you", "Thanks!", "thank you po", "salamat", "salamat po", "ty", "ok", "OK po",
            "okay", "sige", "sige po", "noted", "got it", "alright", "👍", "🙏", "ok 👍",
            "Salamat po! 😊",
        ] {
            assert!(detect(text).is_some(), "'{text}' should be an acknowledgement");
        }
        assert_eq!(detect("thank you"), Some(Ack::Thanks));
        assert_eq!(detect("Sige po"), Some(Ack::Ok));
    }

    #[test]
    fn ignores_real_messages() {
        for text in [
            "",
            "how much is a latte",
            "ok lang ayoko",
            "order 2 latte",
            "noted that the wifi is broken",
            "okay to meet at 3pm?",
            "salamat pero can i ask about the cake",
            "thanks for the menu, what is the price of the matcha?",
        ] {
            assert_eq!(detect(text), None, "'{text}' should not be an acknowledgement");
        }
    }

    #[test]
    fn closing_is_one_short_warm_line() {
        let app = App::for_test();
        for ack in [Ack::Thanks, Ack::Ok] {
            let text = reply_text(&app, "u", ack);
            assert!(!text.contains('\n'), "must be a single line: {text}");
            assert!(symbols(&text) <= 1, "at most one emoji: {text}");
            let lower = text.to_lowercase();
            assert!(!lower.contains("how can i help"), "must not re-ask: {text}");
            assert!(!lower.contains("hours"), "must not repeat the intro: {text}");
            assert!(!lower.contains("wifi"), "must not repeat the intro: {text}");
        }
    }

    #[test]
    fn closing_lines_rotate() {
        let app = App::for_test();
        let first = reply_text(&app, "u", Ack::Ok);
        let second = reply_text(&app, "u", Ack::Ok);
        assert_ne!(first, second, "replies must rotate");
    }

    #[tokio::test]
    async fn closing_during_a_form_reminds_the_next_step() {
        let app = App::for_test();
        app.db.set_form("u", "order_items", &HashMap::new(), 0).unwrap();
        let text = reply_text(&app, "u", Ack::Ok);
        assert!(text.to_lowercase().contains("order"), "{text}");
        assert!(!text.contains('\n'), "{text}");

        app.db.set_form("v", "event_when", &HashMap::new(), 0).unwrap();
        let text = reply_text(&app, "v", Ack::Thanks);
        assert!(text.to_lowercase().contains("booking"), "{text}");
    }
}
