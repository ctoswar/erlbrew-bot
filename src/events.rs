//! Event booking form — a deterministic state machine (NOT LLM-driven).
//!
//! Steps: event_type -> event_when -> guests -> name -> contact -> done.
//! State lives in SQLite (per PSID). The customer may type "cancel" (or similar)
//! at any time. After completion the lead is saved and the team is notified via
//! Telegram; availability, dates and prices are NEVER confirmed by the bot.

use crate::config::Settings;
use crate::db::Db;
use crate::messenger::Messenger;
use crate::faq::FaqStore;
use crate::notify;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub text: String,
    pub quick_replies: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct _State {
    pub step: String,
    pub data: HashMap<String, String>,
    pub tries: i64,
}

pub const TYPE_PAYLOAD_PREFIX: &str = "evt_type:";

/// Map of what the customer can type -> canonical event type.
#[derive(Clone, Debug)]
pub struct EventTypeMap(HashMap<String, String>);

impl EventTypeMap {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert("birthday".into(), "birthday".into());
        map.insert("kaarawan".into(), "birthday".into());
        map.insert("meeting".into(), "meeting".into());
        map.insert("miting".into(), "meeting".into());
        map.insert("photoshoot".into(), "photoshoot".into());
        map.insert("photo shoot".into(), "photoshoot".into());
        map.insert("pictorial".into(), "photoshoot".into());
        map.insert("other".into(), "other".into());
        map.insert("iba".into(), "other".into());
        EventTypeMap(map)
    }

    pub fn lookup(&self, value: &str) -> Option<&str> {
        self.0.get(value).map(|s| s.as_str())
    }
}

/// Pre-built instance so bot code stays the same as Python.
pub static EVENT_TYPE_MAP: EventTypeMap = EventTypeMap::new();

/// ---------------------------------------------------------------------
//  Public API
//  ---------------------------------------------------------------------

/// Return true if the customer currently has an active event form.
pub async fn has_active_form(db: &Db, psid: &str) -> bool {
    db.get_form(psid)
        .await
        .map(|row| row.is_some())
        .unwrap_or(false)
}

/// Re-ask the current step's question (e.g. for images / stickers).
pub async fn current_question(db: &Db, psid: &str) -> Option<Reply> {
    db.get_form(psid).await.ok().map(|row| Reply {
        text: format!("{}?", row.step),
        quick_replies: vec![],
    })
}

/// Start the form from scratch. Sets step = "event_type".
pub async fn start_form(db: &Db, psid: &str) -> Reply {
    db.set_form(psid, "event_type", &HashMap::new(), 0)
        .await
        .ok();
    Reply::new(Q_EVENT_TYPE.clone(), _type_tiles())
}

/// Advance the form with one input. Returns None when there is no form.
pub async fn handle_input(
    messenger: &Arc<Messenger>,
    db: &Db,
    faq: &FaqStore,
    psid: &str,
    raw_value: &str,
) -> Option<Reply> {
    let row = db.get_form(psid).await.ok()?;
    let (step, data, tries) = (row.step.clone(), row.data.clone(), row.tries);

    // Cancel?
    let low = raw_value.to_lowercase().trim().to_string();
    if is_cancel(&low) {
        db.delete_form(psid).await.ok()?;
        return Some(Reply::new(Q_FORM_CANCELLED.clone(), vec![]));
    }

    // Interpret: payload prefix EVT_TYPE: or plain text.
    let intent = interpret(&low);

    match intent {
        // Payload -> advance the form with the given type
        Some("evt_type") => {
            // payload payload could be e.g. "EVT_TYPE:Birthday" but after
            // interpretation we already stripped the prefix; re-use the raw
            // if needed.
            // Actually interpretation returns the canonical name after the prefix.
            // For simplicity, re-extract from raw if it starts with TYPE_PAYLOAD_PREFIX
            if raw_value.to_lowercase().starts_with(TYPE_PAYLOAD_PREFIX) {
                let evt_type = &raw_value[TYPE_PAYLOAD_PREFIX.len()..];
                let canonical = EVENT_TYPE_MAP.lookup(evt_type);
                if let Some(ct) = canonical {
                    // advance to next step
                    let nxt = match step.as_str() {
                        "event_type" => "event_when",
                        "event_when" => "guests",
                        "guests" => "name",
                        "name" => "contact",
                        _ => "event_type",
                    };
                    let mut new_data = data.clone();
                    new_data.insert("type".into(), ct.into());
                    db.set_form(psid, &nxt, &new_data, 0)
                        .await
                        .ok();
                    return Some(_question_for(&nxt));
                }
            }
            // Fall through to plain text interpretation
        }
        Some(step_name) => {
            // Plain text interpretation produced a known keyword / payload.
            // Use it as the input value.
            let parsed = match step_name {
                "event_type" => _interpret_event_type(raw_value),
                "event_when" => _interpret_when(raw_value),
                "guests" => _interpret_guests(raw_value),
                "name" => _interpret_name(raw_value),
                "contact" => _interpret_contact(raw_value),
                _ => None,
            };
            // Actually handle_input does its own _next; we just forward.
            // But we already called interpret above; let's use the simpler path
            // and let _next handle it.
        }
        None => {
            // No interpretation recognized — re-ask with hint.
            // Actually let's just count tries and re-ask.
            // But the Python version calls _next which does its own thing.
        }
    }

    // Default: advance via _next-like logic
    _next_impl(messenger, db, psid, _State { step, data, tries }, raw_value).await
}

/// Cancel check
fn is_cancel(text: &str) -> bool {
    let s = text.trim().to_lowercase();
    const CANCEL_SET: &[&str] = &[
        "cancel", "cancelled", "canceled", "cancel po", "cancel nl", "cancel it",
        "ayoko na", "nevermind", "never mind", "wag na", "wag nalang",
        "cancel event", "cancel booking",
    ];
    CANCEL_SET.iter().any(|&kw| s == kw || s == format!("{} po", kw) || s == format!("{} nl", kw))
}

/// Interpret raw input: return a normalized step name or "cancel".
fn interpret(value: &str) -> Option<&'static str> {
    // First check cancel
    if is_cancel(value) {
        return Some("cancel");
    }
    // Check if it's a payload prefix
    if value.to_lowercase().starts_with(TYPE_PAYLOAD_PREFIX) {
        return Some("evt_type");
    }
    // Then check plain text / keyword
    // The canonical question texts reference EVENT_TYPES keys; we try to match
    for (needle, canonical) in EVENT_TYPE_MAP.0.iter() {
        // Simplified: exact match
        if value.to_lowercase() == needle.to_lowercase() {
            return Some(canonical);
        }
        // Substring match (case-insensitive)
        if value.to_lowercase().contains(needle) {
            return Some(canonical);
        }
    }
    // If it starts with "events" or "booking" it's a new form
    if value.to_lowercase().contains("event") || value.to_lowercase().contains("book") {
        return Some("event_type");
    }
    None
}

/// Plain-text event-type guess from free text
fn _interpret_event_type(value: &str) -> Option<String> {
    let low = value.to_lowercase();
    for (needle, canonical) in EVENT_TYPE_MAP.0.iter() {
        if low.contains(needle) {
            return Some(canonical.into());
        }
    }
    None
}

/// Date/time guess: just validate it has digits
fn _interpret_when(value: &str) -> Option<String> {
    let v = value.trim();
    if v.len() >= 4 && v.len() <= 120 && v.chars().any(|c| c.is_ascii_digit()) {
        return Some(v.into());
    }
    None
}

/// Guests guess: extract first number 1-1000
fn _interpret_guests(value: &str) -> Option<String> {
    let m = regex::Regex::new(r"\d{1,4}").ok()?
        .find(value)?;
    let n: u32 = m.as_str().parse().ok()?;
    if 1 <= n && n <= 1000 {
        return Some(n.to_string());
    }
    None
}

/// Name guess: length 2-120
fn _interpret_name(value: &str) -> Option<String> {
    let v = value.trim();
    if 2 <= v.len() && v.len() <= 120 {
        return Some(v.into());
    }
    None
}

/// Contact guess: lenient PH mobile
fn _interpret_contact(value: &str) -> Option<String> {
    let digits = regex::Regex::new(r"[\s\-().]")
        .ok()?
        .replace_all(value, "");
    let digits = digits.lstrip("+");
    if regex::Regex::new(r"(?:0|63)?9\d{9}")
        .ok()?
        .is_match(&digits)
    {
        return Some(digits);
    }
    None
}

/// Mapping from step -> next step
fn next_step(step: &str) -> &'static str {
    match step {
        "event_type" => "event_when",
        "event_when" => "guests",
        "guests" => "name",
        "name" => "contact",
        _ => "event_type",
    }
}

/// Return the question text + quick replies for a step
fn question_for(step: &str) -> Reply {
    match step {
        "event_type" => Reply {
            text: Q_EVENT_TYPE.clone(),
            quick_replies: _type_tiles(),
        },
        "event_when" => Reply {
            text: Q_EVENT_WHEN.clone(),
            quick_replies: vec![],
        },
        "guests" => Reply {
            text: Q_GUESTS.clone(),
            quick_replies: vec![],
        },
        "name" => Reply {
            text: Q_NAME.clone(),
            quick_replies: vec![],
        },
        "contact" => Reply {
            text: Q_CONTACT.clone(),
            quick_replies: vec![],
        },
        _ => Reply {
            text: Q_FORM_CANCELLED.clone(),
            quick_replies: vec![],
        },
    }
}

/// _next logic: validate `value` for the current step; advance, re-ask or give up.
async fn _next_impl(
    messenger: &Arc<Messenger>,
    db: &Db,
    psid: &str,
    state: _State,
    raw_value: &str,
) -> Option<Reply> {
    let step = state.step;
    let data = state.data;
    let tries = state.tries + 1; // bump first time we call validation

    // Cancel?
    if is_cancel(raw_value) {
        db.delete_form(psid).await.ok()?;
        return Some(Reply::new(Q_FORM_CANCELLED.clone(), vec![]));
    }

    // Validate based on step
    let parsed = match step.as_str() {
        "event_type" => _val_type(raw_value),
        "event_when" => _val_when(raw_value),
        "guests" => _val_guests(raw_value),
        "name" => _val_name(raw_value),
        "contact" => _val_contact(raw_value),
        _ => None,
    };

    let Some(parsed) = parsed else {
        // re-ask
        db.set_form(psid, &step, &data, tries).await.ok()?;
        let hint = _HINTS.get(step).map(|h| h.clone()).unwrap_or_default();
        return Some(Reply::new(format!("{}{}REASK_PREFIX", Q_REASK_PREFIX, hint), vec![]));
    };

    // Store the parsed value
    let key = step.strip_prefix("event_").unwrap_or(step);
    let mut new_data = data.clone();
    new_data.insert(key.into(), parsed.clone());

    if step == "contact" {
        return Some(_finish_impl(messenger, db, psid, &new_data).await);
    }

    let nxt = next_step(step);
    db.set_form(psid, nxt, &new_data, 0)
        .await
        .ok();
    Some(question_for(nxt))
}

/// _finish: save lead, notify, clear form.
async fn _finish_impl(
    messenger: &Arc<Messenger>,
    db: &Db,
    psid: &str,
    data: &HashMap<String, String>,
) -> Reply {
    let guests: i64 = data
        .get("guests")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let lead_id = db
        .add_lead(
            psid,
            data.get("type").cloned().unwrap_or_else(|| "other".into()),
            data.get("when").cloned().unwrap_or_default(),
            guests,
            data.get("name").cloned().unwrap_or_default(),
            data.get("contact").cloned().unwrap_or_default(),
        )
        .await
        .unwrap_or(0);

    db.delete_form(psid).await.ok();

    // Telegram notification
    let _ = notify::notify_event_lead(
        messenger,
        db,
        psid,
        data.get("type").cloned().unwrap_or_else(|| "Other".into()),
        data.get("when").cloned().unwrap_or_default(),
        guests,
        data.get("name").cloned().unwrap_or_default(),
        data.get("contact").cloned().unwrap_or_default(),
    );

    let hours = data
        .get("event_reply_hours")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(24);

    Reply {
        text: Q_FORM_DONE.format(
            data.get("name").cloned().unwrap_or_default(),
            data.get("type").cloned().unwrap_or_else(|| "Other".into()),
            data.get("when").cloned().unwrap_or_default(),
            data.get("guests").cloned().unwrap_or_else(|| "?"),
            data.get("contact").cloned().unwrap_or_default(),
            hours,
        ),
        quick_replies: vec![],
    }
}

/// Validators

fn _val_type(value: &str) -> Option<String> {
    let low = value.to_lowercase();
    if low.starts_with(TYPE_PAYLOAD_PREFIX) {
        let evt = &low[TYPE_PAYLOAD_PREFIX.len()..];
        EVENT_TYPE_MAP.lookup(evt).map(|s| s.into())
    } else {
        // plain text substring match
        for (needle, canonical) in EVENT_TYPE_MAP.0.iter() {
            if low.contains(needle) {
                return Some(canonical.into());
            }
        }
        None
    }
}

fn _val_when(value: &str) -> Option<String> {
    let v = value.trim();
    if 4 <= v.len() && v.len() <= 120 && v.chars().any(|c| c.is_ascii_digit()) {
        return Some(v.into());
    }
    None
}

fn _val_guests(value: &str) -> Option<String> {
    let m = regex::Regex::new(r"\d{1,4}").ok()?
        .find(value)?;
    let n: u32 = m.as_str().parse().ok()?;
    if 1 <= n && n <= 1000 {
        return Some(n.to_string());
    }
    None
}

fn _val_name(value: &str) -> Option<String> {
    let v = value.trim();
    if 2 <= v.len() && v.len() <= 120 {
        return Some(v.into());
    }
    None
}

fn _val_contact(value: &str) -> Option<String> {
    let digits = regex::Regex::new(r"[\s\-().]")
        .ok()?
        .replace_all(value, "");
    let digits = digits.lstrip("+");
    if regex::Regex::new(r"(?:0|63)?9\d{9}")
        .ok()?
        .is_match(&digits)
    {
        return Some(digits);
    }
    None
}

// --------------------------------------------------------------------------
// Question texts (Taglish, [PLACEHOLDERS] for the owner)
// --------------------------------------------------------------------------

static Q_EVENT_TYPE: &str = "[PLACEHOLDER: intro line kung gusto nyo — e.g. Sige po! Mag-book tayo ng event! 🎉] \
Anong klase po ng event?";

static Q_EVENT_TYPE_HINT: &str = "Piliin lang po natin: Birthday 🎂, Meeting 🧾, Photoshoot 📸, o Other ✨. \
(Pwdi rin po kayo mag-type ng 'cancel' kahit anong oras para tumigil.)";

static Q_EVENT_WHEN: &str = "Anong petsa at oras po? (e.g. 'May 12, 3 PM' o 'Sabado 9 AM')\nPAALALA po: hindi pa po ito confirmation ng schedule — \
ang team namin po ang tatawag mag-confirm ng availability. 😊";

static Q_EVENT_WHEN_HINT: &str = "Paki-type po ang petsa at oras, e.g. 'May 12 3 PM' o 'Sabado 9 AM'. \
('cancel' kung gusto nyo po tumigil)";

static Q_GUESTS: &str = "Ilang guests po ang ine-expect nyo? (number lang po, e.g. 12)";

static Q_GUESTS_HINT: &str = "Paki-type lang po ang bilang ng guests as a number po, e.g. 12. 😊";

static Q_NAME: &str = "Sige po! Anong pangalan po natin for the booking?";

static Q_NAME_HINT: &str = "Paki-type lang po ang pangalan (o pangalan ng group), e.g. 'Maria Santos'. 😊";

static Q_CONTACT: &str = "Anong mobile number po natin? (para po sa updates, e.g. 0917-123-4567 \
o +639171234567)";

static Q_CONTACT_HINT: &str = "Pasensya po — number po ba? Pakitype po ng PH mobile number, e.g. 09171234567. \
('cancel' kung ayaw nyo na po)";

static Q_FORM_DONE: &str = "Salamat po, {name}! 🎉 Naipasa na po namin ang booking nyo:\n\
• Type: {etype}\n\
• Petsa/oras: {when}\n\
• Guests: {guests}\n\
• Pangalan: {name}\n\
• Number: {contact}\n\
\n\
Sasagot po ang aming team within {hours} oras dito sa Messenger. \
PAALALA po: hindi po ito confirmation ng availability, petsa o presyo — \
ang team po namin ang magsasabi kung available ang petsa at kung magkano. \
Salamat po! ☕";

static Q_FORM_CANCELLED: &str = "Sige po — kinansel ko na po muna ang booking form. \
I-type lang po ang tanong nyo dito, o tap 'Events' ulit kapag ready na po kayo. 😊";

static Q_FORM_GAVE_UP: &str = "Hindi ko na po maituloy ang form, kinansel ko po muna ito 😅 \
— i-type na lang po ang tanong nyo dito, o tap 'Events' ulit kapag ready na po kayo.";

static Q_REASK_PREFIX: &str = "Pasensya po — ";

// Quick-reply tiles for the event type question
static _TYPE_TILES: [(&str, &str); 4] = [
    ("Birthday 🎂", "evt_type:birthday"),
    ("Meeting 🧾", "evt_type:meeting"),
    ("Photoshoot 📸", "evt_type:photoshoot"),
    ("Other ✨", "evt_type:other"),
];

/// Return the tiles as Vec<(title, payload)> for Messenger.
fn _type_tiles() -> Vec<(String, String)> {
    _TYPE_TILES
        .iter()
        .map(|(t, p)| (t.into(), p.into()))
        .collect()
}

// --------------------------------------------------------------------------
// _HINTS
// --------------------------------------------------------------------------

static _HINTS: &[(&str, &str)] = &[
    ("event_type", &Q_EVENT_TYPE_HINT),
    ("event_when", &Q_EVENT_WHEN_HINT),
    ("guests", &Q_GUESTS_HINT),
    ("name", &Q_NAME_HINT),
    ("contact", &Q_CONTACT_HINT),
];

// --------------------------------------------------------------------------
// Global constants placed here so modules share them
// --------------------------------------------------------------------------

const Q_REASK_PREFIX: &str = "Pasensya po — ";

// --------------------------------------------------------------------------
// Route payload back to form
// --------------------------------------------------------------------------

// A postback/event payload like "EVT_TYPE:Birthday" goes into the form chain.
// For the axum routes, route_payload forwards to the form handler.
pub async fn route_payload(
    messenger: &Arc<Messenger>,
    db: &Db,
    faq: &FaqStore,
    psid: &str,
    payload: &str,
) -> bool {
    let p = payload.trim().to_uppercase();
    // EVT_TYPE: payloads
    if let Some(stripped) = p.strip_prefix("EVT_TYPE:") {
        let evt_type = stripped.to_lowercase();
        let canonical = EVENT_TYPE_MAP.lookup(&evt_type);
        if let Some(ct) = canonical {
            // treat it as a normal input step
            let row = db.get_form(psid).await.ok()?;
            let (step, data, tries) = (row.step.clone(), row.data.clone(), row.tries);
            let mut new_data = data.clone();
            new_data.insert("type".into(), ct.into());
            let nxt = match step.as_str() {
                "event_type" => "event_when",
                "event_when" => "guests",
                "guests" => "name",
                "name" => "contact",
                _ => "event_type",
            };
            db.set_form(psid, &nxt, &new_data, 0)
                .await
                .ok();
            // Send next question
            let reply = question_for(nxt);
            let _ = messenger
                .send_text(psid, &reply.text)
                .await;
            if !reply.quick_replies.is_empty() {
                // Messenger will display quick replies based on the text+payload pair
            }
            return true;
        }
    }
    // Unknown payload -> reset to help
    if let Some(help_entry) = faq.entry_by_id("fallback_help") {
        // Just return true; the caller can send the help text
    }
    false
}