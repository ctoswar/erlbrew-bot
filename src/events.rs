//! Event booking form — a deterministic state machine (NOT LLM-driven).
//!
//! Steps: event_type -> event_when -> event_guests -> event_name -> event_contact -> done.
//! State lives in SQLite (per PSID). The customer may type "cancel" (or similar) at any time.
//! After completion the lead is saved and the team is notified via Telegram; availability,
//! dates and prices are NEVER confirmed by the bot.

use crate::forms::{clean_len, clean_name, clean_ph_mobile, fill, is_cancel, kind_of_step, qr, FormKind, Reply, MAX_TRIES};
use crate::db::FormRow;
use crate::state::App;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tracing::error;

pub const TYPE_PAYLOAD_PREFIX: &str = "EVT_TYPE:";

/// What the customer can type -> canonical event type (ordered, first match wins).
const EVENT_TYPES: &[(&str, &str)] = &[
    ("birthday", "birthday"),
    ("kaarawan", "birthday"),
    ("meeting", "meeting"),
    ("miting", "meeting"),
    ("photoshoot", "photoshoot"),
    ("photo shoot", "photoshoot"),
    ("pictorial", "photoshoot"),
    ("other", "other"),
    ("iba", "other"),
];

static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{1,4}").unwrap());

// --------------------------------------------------------------------------
// Public API
// --------------------------------------------------------------------------

/// Start (or restart) the form. Sets step = "event_type".
pub fn start_form(app: &App, psid: &str) -> Reply {
    if let Err(err) = app.db.set_form(psid, "event_type", &HashMap::new(), 0) {
        error!(error = %err, "could not start event form");
    }
    question_for("event_type")
}

/// Re-ask the question for the stored step (used for stickers/images).
pub fn current_question(row: &FormRow) -> Reply {
    question_for(&row.step)
}

/// Advance the form with one input. Returns None when there is no active event form.
pub async fn handle_input(app: &App, psid: &str, raw: &str) -> Option<Reply> {
    let row = app.db.get_form(psid).ok().flatten()?;
    if kind_of_step(&row.step) != Some(FormKind::Event) {
        return None;
    }

    if is_cancel(raw) {
        let _ = app.db.delete_form(psid);
        return Some(Reply::text(Q_FORM_CANCELLED));
    }

    let Some(value) = validate(&row.step, raw) else {
        return Some(reask(app, psid, &row));
    };

    let key = row.step.strip_prefix("event_").unwrap_or(&row.step);
    let mut data = row.data.clone();
    data.insert(key.to_string(), value);

    if row.step == "event_contact" {
        return Some(finish(app, psid, &data).await);
    }

    let next = next_step(&row.step);
    if let Err(err) = app.db.set_form(psid, next, &data, 0) {
        error!(error = %err, "could not save event form step");
    }
    Some(question_for(next))
}

// --------------------------------------------------------------------------
// Internals
// --------------------------------------------------------------------------

/// Gentle reminder of the next step, used when the customer sends an acknowledgement while the
/// event form is still open.
pub fn step_hint(step: &str) -> &'static str {
    match step {
        "event_type" => "Para sa booking, i-tap lang po ang tipo ng event 👇",
        "event_when" => "Para sa booking, isulat lang po ang petsa at oras 👇",
        "event_guests" => "Para sa booking, ilagay lang po ang bilang ng guests 👇",
        "event_name" => "Para sa booking, isulat lang po ang pangalan 👇",
        "event_contact" => "Para sa booking, isulat lang po ang mobile number 👇",
        _ => "Para sa booking, i-continue lang po natin ang form 👇",
    }
}

fn next_step(step: &str) -> &'static str {
    match step {
        "event_type" => "event_when",
        "event_when" => "event_guests",
        "event_guests" => "event_name",
        "event_name" => "event_contact",
        _ => "event_type",
    }
}

fn reask(app: &App, psid: &str, row: &FormRow) -> Reply {
    let tries = row.tries + 1;
    if tries >= MAX_TRIES {
        let _ = app.db.delete_form(psid);
        return Reply::text(Q_FORM_GAVE_UP);
    }
    if let Err(err) = app.db.set_form(psid, &row.step, &row.data, tries) {
        error!(error = %err, "could not record failed attempt");
    }
    let hint = match row.step.as_str() {
        "event_type" => Q_EVENT_TYPE_HINT,
        "event_when" => Q_EVENT_WHEN_HINT,
        "event_guests" => Q_GUESTS_HINT,
        "event_name" => Q_NAME_HINT,
        _ => Q_CONTACT_HINT,
    };
    let mut reply = Reply::text(format!("{Q_REASK_PREFIX}{hint}"));
    if row.step == "event_type" {
        reply.quick_replies = type_tiles();
    }
    reply
}

fn question_for(step: &str) -> Reply {
    match step {
        "event_type" => Reply::new(Q_EVENT_TYPE, type_tiles()),
        "event_when" => Reply::text(Q_EVENT_WHEN),
        "event_guests" => Reply::text(Q_GUESTS),
        "event_name" => Reply::text(Q_NAME),
        "event_contact" => Reply::text(Q_CONTACT),
        _ => Reply::text(Q_FORM_CANCELLED),
    }
}

fn type_tiles() -> Vec<(String, String)> {
    vec![
        qr("Birthday 🎂", "EVT_TYPE:BIRTHDAY"),
        qr("Meeting 🧾", "EVT_TYPE:MEETING"),
        qr("Photoshoot 📸", "EVT_TYPE:PHOTOSHOOT"),
        qr("Other ✨", "EVT_TYPE:OTHER"),
    ]
}

fn validate(step: &str, raw: &str) -> Option<String> {
    match step {
        "event_type" => val_type(raw),
        "event_when" => {
            let v = clean_len(raw, 4, 120)?;
            v.chars().any(|c| c.is_ascii_digit() || c.is_alphabetic()).then_some(v)
        }
        "event_guests" => {
            let n: u32 = DIGITS.find(raw)?.as_str().parse().ok()?;
            (1..=1000).contains(&n).then(|| n.to_string())
        }
        "event_name" => clean_name(raw),
        "event_contact" => clean_ph_mobile(raw),
        _ => None,
    }
}

/// Accepts the quick-reply payload ("EVT_TYPE:BIRTHDAY") or free text ("birthday po").
fn val_type(raw: &str) -> Option<String> {
    let low = raw.trim().to_lowercase();
    if let Some(rest) = low.strip_prefix(&TYPE_PAYLOAD_PREFIX.to_lowercase()) {
        let rest = rest.trim();
        return EVENT_TYPES.iter().find(|(needle, _)| *needle == rest).map(|(_, c)| c.to_string());
    }
    let tokens: Vec<&str> = low.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    EVENT_TYPES
        .iter()
        .find(|(needle, _)| {
            if needle.contains(' ') { low.contains(needle) } else { tokens.contains(needle) }
        })
        .map(|(_, c)| c.to_string())
}

async fn finish(app: &App, psid: &str, data: &HashMap<String, String>) -> Reply {
    let get = |k: &str| data.get(k).cloned().unwrap_or_default();
    let (etype, when, name, contact) = (get("type"), get("when"), get("name"), get("contact"));
    let guests: i64 = get("guests").parse().unwrap_or(0);

    let lead_id = match app.db.add_lead(psid, &etype, &when, guests, &name, &contact) {
        Ok(id) => id,
        Err(err) => {
            error!(error = %err, "could not save event lead");
            0
        }
    };
    let _ = app.db.delete_form(psid);

    // Notify the team in the background so the customer gets their reply right away.
    let notifier = app.notifier.clone();
    let (p, e, w, n, c) = (psid.to_string(), etype.clone(), when.clone(), name.clone(), contact.clone());
    tokio::spawn(async move {
        notifier.notify_event_lead(&p, lead_id, &e, &w, guests, &n, &c).await;
    });

    let hours = app.settings.event_reply_hours.to_string();
    Reply::text(fill(
        Q_FORM_DONE,
        &[
            ("name", &name),
            ("etype", &etype),
            ("when", &when),
            ("guests", &guests.to_string()),
            ("contact", &contact),
            ("hours", &hours),
        ],
    ))
}

// --------------------------------------------------------------------------
// Question texts (Taglish)
// --------------------------------------------------------------------------

const Q_EVENT_TYPE: &str = "Sige po! Mag-book tayo ng event! 🎉 Anong klase po ng event?";
const Q_EVENT_TYPE_HINT: &str = "Piliin lang po natin: Birthday 🎂, Meeting 🧾, Photoshoot 📸, o Other ✨. \
(Pwede rin po kayong mag-type ng 'cancel' kahit anong oras para tumigil.)";
const Q_EVENT_WHEN: &str = "Anong petsa at oras po? (e.g. 'May 12, 3 PM' o 'Sabado 9 AM')\n\
PAALALA po: hindi pa po ito confirmation ng schedule — ang team namin po ang magko-confirm ng availability. 😊";
const Q_EVENT_WHEN_HINT: &str = "Paki-type po ang petsa at oras, e.g. 'May 12 3 PM' o 'Sabado 9 AM'. \
('cancel' kung gusto nyo po tumigil)";
const Q_GUESTS: &str = "Ilang guests po ang ine-expect nyo? (number lang po, e.g. 12)";
const Q_GUESTS_HINT: &str = "Paki-type lang po ang bilang ng guests as a number po, e.g. 12. 😊";
const Q_NAME: &str = "Sige po! Anong pangalan po natin for the booking?";
const Q_NAME_HINT: &str = "Paki-type lang po ang pangalan (o pangalan ng group), e.g. 'Maria Santos'. 😊";
const Q_CONTACT: &str = "Anong mobile number po natin? (para po sa updates, e.g. 0917-123-4567 o +639171234567)";
const Q_CONTACT_HINT: &str = "number po ba? Pakitype po ng PH mobile number, e.g. 09171234567. \
('cancel' kung ayaw nyo na po)";
const Q_REASK_PREFIX: &str = "Pasensya po — ";

const Q_FORM_DONE: &str = "Salamat po, {name}! 🎉 Naipasa na po namin ang booking nyo:\n\
• Type: {etype}\n\
• Petsa/oras: {when}\n\
• Guests: {guests}\n\
• Pangalan: {name}\n\
• Number: {contact}\n\n\
Sasagot po ang aming team within {hours} oras dito sa Messenger. \
PAALALA po: hindi po ito confirmation ng availability, petsa o presyo — \
ang team po namin ang magsasabi kung available ang petsa at kung magkano. Salamat po! ☕";

const Q_FORM_CANCELLED: &str = "Sige po — kinansel ko na po muna ang booking form. \
I-type lang po ang tanong nyo dito, o tap 'Events' ulit kapag ready na po kayo. 😊";
const Q_FORM_GAVE_UP: &str = "Hindi ko na po maituloy ang form, kinansel ko po muna ito 😅 \
— i-type na lang po ang tanong nyo dito, o tap 'Events' ulit kapag ready na po kayo.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_detection() {
        assert_eq!(val_type("EVT_TYPE:BIRTHDAY").as_deref(), Some("birthday"));
        assert_eq!(val_type("birthday po").as_deref(), Some("birthday"));
        assert_eq!(val_type("photo shoot sana").as_deref(), Some("photoshoot"));
        assert_eq!(val_type("kaibigan ko"), None, "'iba' must not match inside 'kaibigan'");
        assert_eq!(val_type("EVT_TYPE:NOPE"), None);
    }

    #[test]
    fn validators() {
        assert_eq!(validate("event_guests", "mga 12 po").as_deref(), Some("12"));
        assert_eq!(validate("event_guests", "5000"), None);
        assert_eq!(validate("event_when", "May 12, 3 PM").as_deref(), Some("May 12, 3 PM"));
        assert_eq!(validate("event_when", "hm"), None);
        assert_eq!(validate("event_contact", "0917 123 4567").as_deref(), Some("09171234567"));
    }
}
