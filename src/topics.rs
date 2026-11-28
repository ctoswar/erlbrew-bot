//! Deterministic answers for the loyalty program, private events and table reservations.
//!
//! These topics are answered by the bot itself — it never invents stamps, points, rewards,
//! prices, packages, capacity or availability. Anything outside the scripted scope (lost
//! points, a missing stamp, an app problem) is handed off to the team.

use crate::forms::fill;
use crate::state::App;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    /// Loyalty program: app, stamps, points, rewards.
    Loyalty,
    /// Private events: birthdays, meetings, photoshoots, packages.
    PrivateEvents,
    /// Regular seating: walk-in only, no table reservations.
    Reservations,
    /// Outside the scripted scope (lost points, app problem) — the team must answer.
    NeedsTeam,
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// "loyalty", "stamps", "rewards" — unambiguous.
static LOYALTY_STRONG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(loyalt(?:y|ies)|stamps?|rewards?)\b").expect("loyalty regex")
});
/// "point(s)" — but not a "meeting point".
static LOYALTY_POINT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bpoints?\b").expect("loyalty point regex"));
static NOT_LOYALTY_POINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(meeting|pick\s?-?up|collection|assembly|reference|starting|end)\s+points?\b",
    )
    .expect("point guard regex")
});

/// Lost points, missing stamp, app problem — the bot must not answer these.
static LOYALTY_PROBLEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(nawala|lost|missing|may\s+problema|may\s+error|error|bug|bali|di\s+gumagana|hindi\s+gumagana|not\s+working|doesn'?t\s+work|naka-?credit|nacredit|nacredited|credited|na-?update|nagsave|na-?save|nasa-?save|sinave)\b",
    )
    .expect("loyalty problem regex")
});
/// The loyalty app itself.
static APP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(app|application)\b").expect("app regex"));

/// Private events, birthdays, meetings, photoshoots, celebrations.
static PRIVATE_EVENTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(events?|birthdays?|meetings?|photoshoots?|photo\s+shoots?|pictorials?|celebrations?|salubong|despedida|private\s+(?:events?|bookings?|parties?))\b",
    )
    .expect("private events regex")
});

/// "package" only counts when it is asked about (a coffee package is not an event).
static PACKAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bpackages?\b").expect("package regex"));
static PACKAGE_QUESTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(magkano|how\s+much|price|prices|pricing|cost|costs|rate|rates|available|availability|ano\s+ang|ano\s+po|ano\s+ba)\b",
    )
    .expect("package question regex")
});

/// The noun "reservation" is unambiguous.
static RESERVATION_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\breservations?\b").expect("reservation regex"));
/// The verbs only mean regular seating when a table/seat is mentioned.
static RESERVE_VERB_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(reserve|mag-?reserve|i-?reserve|hold|mag-?hold|i-?hold)\b")
        .expect("reserve verb regex")
});
static SEATING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(tables?|seats?|chairs?|regular\s+seating|walk-?in|dining)\b")
        .expect("seating regex")
});

fn is_loyalty(low: &str) -> bool {
    if LOYALTY_STRONG_RE.is_match(low) {
        return true;
    }
    LOYALTY_POINT_RE.is_match(low) && !NOT_LOYALTY_POINT_RE.is_match(low)
}

fn asks_about_package(low: &str, raw: &str) -> bool {
    PACKAGE_RE.is_match(low)
        && (PACKAGE_QUESTION_RE.is_match(low) || low.contains(" ba") || raw.contains('?'))
}

/// Regular-seating reservation question ("pwede mag-reserve ng table?").
pub fn is_reservation(text: &str) -> bool {
    let low = text.to_lowercase();
    if RESERVATION_WORD_RE.is_match(&low) {
        return true;
    }
    RESERVE_VERB_RE.is_match(&low) && SEATING_RE.is_match(&low)
}

/// Loyalty / private-events topic (reservations are handled separately, before the booking
/// intent, so "mag-reserve ng table" never opens the event form).
pub fn detect(text: &str) -> Option<Topic> {
    let low = text.to_lowercase();
    let problem = LOYALTY_PROBLEM_RE.is_match(&low);
    if is_loyalty(&low) {
        return Some(if problem { Topic::NeedsTeam } else { Topic::Loyalty });
    }
    // A problem with the (loyalty) app is for the team to check.
    if problem && APP_RE.is_match(&low) {
        return Some(Topic::NeedsTeam);
    }
    if PRIVATE_EVENTS_RE.is_match(&low) || asks_about_package(&low, text) {
        return Some(Topic::PrivateEvents);
    }
    None
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

const LOYALTY_APP_URL: &str = "https://app.erlbrewcafe.page/";
const LOYALTY_TEXT: &str =
    "Meron po! ☕ Gamitin lang po ang Erlbrew app para sa loyalty rewards natin: ";

/// Owner note: fill this in once the real stamp/reward policy is final, e.g.
/// "Every ₱__ purchase = 1 stamp, __ stamps = free drink".
/// Keep it empty until then — the bot must never invent stamps, points or rewards.
const LOYALTY_POLICY: &str = "";

const PRIVATE_EVENTS_TEXT: &str = "Oo po! 🎉 Nagho-host po kami sa Erlbrew ng private events:\n\
🎂 Birthdays · 🧾 Meetings · 📸 Photoshoots\n\
I-tap lang po ang 'Start booking' sa baba 👇 at ang aming team po ang magse-send ng packages at details within {hours} hours. Salamat po!";

const RESERVATIONS_TEXT: &str = "Walang reservation po for regular seating, walk-in lang po kami. \
First come, first served po 😊 Para sa mga event o malaking group, i-tap lang po ang 'Start booking' 👇";

/// The reply text for a topic.
pub fn reply_text(app: &App, topic: Topic) -> String {
    match topic {
        Topic::Loyalty => {
            let mut text = format!("{LOYALTY_TEXT}{LOYALTY_APP_URL}");
            if !LOYALTY_POLICY.trim().is_empty() {
                text.push(' ');
                text.push_str(LOYALTY_POLICY.trim());
            }
            text
        }
        Topic::PrivateEvents => fill(
            PRIVATE_EVENTS_TEXT,
            &[("hours", &app.settings.event_reply_hours.to_string())],
        ),
        Topic::Reservations => RESERVATIONS_TEXT.to_string(),
        Topic::NeedsTeam => String::new(),
    }
}

/// The reply to send for a topic.
pub fn reply(app: &App, topic: Topic) -> crate::forms::Reply {
    crate::forms::Reply::text(reply_text(app, topic))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::state::App;

    #[test]
    fn detects_reservation_questions() {
        for text in [
            "pwede mag-reserve ng table?",
            "may reservation ba?",
            "pwede ba i-hold yung seats?",
            "i-hold po sana ng table for 4",
            "may reservation kayo for regular seating?",
        ] {
            assert!(is_reservation(text), "'{text}' should be a reservation question");
        }
        for text in ["mag-book po kayo for birthday", "ok lang", "2 iced latte"] {
            assert!(!is_reservation(text), "'{text}' should not be a reservation question");
        }
    }

    #[test]
    fn detects_loyalty_questions() {
        for text in [
            "may loyalty card ba kayo?",
            "ano po ang stamps?",
            "how do I earn points?",
            "may rewards ba kayo?",
            "loyalty app",
        ] {
            assert_eq!(detect(text), Some(Topic::Loyalty), "'{text}'");
        }
        // A "meeting point" is not about loyalty.
        assert_ne!(detect("ano po ang meeting point?"), Some(Topic::Loyalty));
    }

    #[test]
    fn loyalty_problems_go_to_the_team() {
        for text in [
            "nawala po yung points ko",
            "missing yung stamp ko",
            "may problema sa app",
            "hindi naka-credit yung points",
            "di gumagana yung app",
        ] {
            assert_eq!(detect(text), Some(Topic::NeedsTeam), "'{text}'");
        }
    }

    #[test]
    fn detects_private_event_questions() {
        for text in [
            "may events ba kayo?",
            "do you host private events?",
            "may birthday packages?",
            "magkano po ang package?",
            "photoshoot ba possible?",
            "ano ang meeting packages?",
        ] {
            assert_eq!(detect(text), Some(Topic::PrivateEvents), "'{text}'");
        }
        // Not a question about packages -> not this topic.
        assert_eq!(detect("2 iced latte package"), None);
    }

    #[test]
    fn replies_never_invent_details() {
        let app = App::for_test();

        let loyalty = reply_text(&app, Topic::Loyalty);
        assert!(loyalty.contains("app.erlbrewcafe.page"), "{loyalty}");
        assert!(!loyalty.contains("1 stamp"), "{loyalty}");
        assert!(!loyalty.contains("free drink"), "{loyalty}");

        let events = reply_text(&app, Topic::PrivateEvents);
        assert!(events.contains("Start booking"), "{events}");
        assert!(!events.contains('₱'), "no prices: {events}");
        assert!(!events.to_lowercase().contains("pax"), "no capacity: {events}");
        assert!(!events.to_lowercase().contains("available"), "no availability: {events}");
        assert!(events.contains("24 hours"), "{events}");

        let reservation = reply_text(&app, Topic::Reservations);
        assert!(reservation.to_lowercase().contains("walk-in"), "{reservation}");
        assert!(reservation.to_lowercase().contains("first come"), "{reservation}");
        assert!(!reservation.to_lowercase().contains("naka-hold"), "{reservation}");
        assert!(!reservation.to_lowercase().contains("ipag-hold"), "{reservation}");
    }

    #[test]
    fn event_reply_uses_the_configured_hours() {
        let mut settings = Settings::for_test();
        settings.event_reply_hours = 12;
        let app = App::for_test_with(settings);
        assert!(reply_text(&app, Topic::PrivateEvents).contains("12 hours"));
    }
}
