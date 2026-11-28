//! Helpers shared by the two deterministic form state machines (events, orders).

use crate::db::Db;
use regex::Regex;
use std::sync::LazyLock;

/// A bot reply: text plus optional (title, payload) quick-reply buttons.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    pub quick_replies: Vec<(String, String)>,
}

impl Reply {
    pub fn new(text: impl Into<String>, quick_replies: Vec<(String, String)>) -> Self {
        Self { text: text.into(), quick_replies }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::new(text, Vec::new())
    }
}

pub fn qr(title: &str, payload: &str) -> (String, String) {
    (title.to_string(), payload.to_string())
}

/// After this many invalid answers in a row the form is abandoned.
pub const MAX_TRIES: i64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormKind {
    Event,
    Order,
}

/// Which form (if any) the stored step belongs to.
pub fn kind_of_step(step: &str) -> Option<FormKind> {
    if step.starts_with("order_") {
        Some(FormKind::Order)
    } else if step.starts_with("event_") {
        Some(FormKind::Event)
    } else {
        None
    }
}

/// The active form for this customer. A form with an unknown step is stale and removed.
pub fn active_kind(db: &Db, psid: &str) -> Option<FormKind> {
    let row = db.get_form(psid).ok().flatten()?;
    match kind_of_step(&row.step) {
        Some(kind) => Some(kind),
        None => {
            let _ = db.delete_form(psid);
            None
        }
    }
}

const CANCEL_WORDS: &[&str] = &[
    "cancel", "cancelled", "canceled", "cancel po", "cancel nl", "cancel it", "cancel order",
    "cancel event", "cancel booking", "ayoko na", "ayaw ko na", "nevermind", "never mind",
    "wag na", "wag nalang", "wag na po", "not now", "next time",
];

/// Lowercase, trim, and drop trailing punctuation/emoji so "Cancel po!" == "cancel po".
fn squash(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_string()
}

/// "cancel", "cancel po", "wag na", "cancel nyo na po" ... but not "cancellation policy?".
pub fn is_cancel(raw: &str) -> bool {
    let s = squash(raw);
    CANCEL_WORDS.iter().any(|kw| {
        s == *kw || s.strip_prefix(kw).is_some_and(|rest| rest.starts_with(' '))
    })
}

/// Normalise free text for yes/no style matching ("Tama po ✅" -> "tama po").
pub fn plain(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replace `{key}` placeholders.
pub fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

/// Trimmed text whose length (in characters) is within `min..=max`.
pub fn clean_len(value: &str, min: usize, max: usize) -> Option<String> {
    let v = value.trim();
    let n = v.chars().count();
    (min..=max).contains(&n).then(|| v.to_string())
}

pub fn clean_name(value: &str) -> Option<String> {
    clean_len(value, 2, 120)
}

static PH_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s\-().]").unwrap());
static PH_MOBILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:0|63)?9\d{9}$").unwrap());

/// Lenient PH mobile number -> canonical `09XXXXXXXXX`.
pub fn clean_ph_mobile(value: &str) -> Option<String> {
    let stripped = PH_SEPARATORS.replace_all(value.trim(), "");
    let digits = stripped.trim_start_matches('+');
    if !PH_MOBILE.is_match(digits) {
        return None;
    }
    let national = digits
        .strip_prefix("63")
        .or_else(|| digits.strip_prefix('0'))
        .unwrap_or(digits);
    Some(format!("0{national}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_detection() {
        assert!(is_cancel("cancel"));
        assert!(is_cancel("Cancel po!"));
        assert!(is_cancel("wag na"));
        assert!(is_cancel("cancel nyo na po"));
        assert!(!is_cancel("cancellation policy?"));
        assert!(!is_cancel("2 iced latte"));
    }

    #[test]
    fn ph_mobile() {
        assert_eq!(clean_ph_mobile("0917-123-4567").as_deref(), Some("09171234567"));
        assert_eq!(clean_ph_mobile("+63 917 123 4567").as_deref(), Some("09171234567"));
        assert_eq!(clean_ph_mobile("9171234567").as_deref(), Some("09171234567"));
        assert_eq!(clean_ph_mobile("12345"), None);
        assert_eq!(clean_ph_mobile("09171234567999"), None);
    }

    #[test]
    fn fill_and_plain() {
        assert_eq!(fill("Hi {name}!", &[("name", "Ana")]), "Hi Ana!");
        assert_eq!(plain("Tama po ✅"), "tama po");
    }
}
