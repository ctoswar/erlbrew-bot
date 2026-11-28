//! Order intake form — a deterministic state machine (NOT LLM-driven).
//!
//! Steps: order_items -> order_confirm -> order_name -> order_address -> order_contact -> done.
//! State lives in the same SQLite form slot as the event form (per PSID); the pipeline
//! dispatches here whenever the active step starts with "order_". The customer may type
//! "cancel" (or similar) at any time.
//!
//! After the items step the bot shows a priced RECEIPT built from the live POS menu: real
//! prices, real variant sizes. The total is an ESTIMATE — the team still confirms the final
//! total in chat. Payment methods and delivery schedules are NEVER confirmed by the bot.
//! The completed order is saved to the `orders` table and the team is notified via Telegram.

use crate::db::FormRow;
use crate::forms::{
    clean_len, clean_name, clean_ph_mobile, fill, is_cancel, kind_of_step, plain, qr, FormKind, Reply,
    MAX_TRIES,
};
use crate::menu::fmt_price;
use crate::state::App;
use std::collections::HashMap;
use tracing::{error, info};

pub const PAYLOAD_OK: &str = "ORD_OK";
pub const PAYLOAD_EDIT: &str = "ORD_EDIT";

#[derive(Debug, PartialEq, Eq)]
enum Confirm {
    Ok,
    Edit,
    Unknown,
}

const OK_WORDS: &[&str] = &[
    "ord ok", "tama", "tama po", "tama na", "oo", "oo po", "opo", "yes", "yes po", "sige",
    "sige po", "ok", "ok po", "okay", "okay po", "correct", "ayos", "ayos lang", "ayos lang po",
    "go", "sure",
];
const EDIT_WORDS: &[&str] = &[
    "ord edit", "edit", "mali", "mali po", "palit", "palit po", "papalitan", "papalitan po",
    "papalatan", "papalatan po", "may papalit", "hindi", "hindi po", "no", "wrong", "change",
];

fn confirm_intent(raw: &str) -> Confirm {
    let p = plain(raw);
    if OK_WORDS.contains(&p.as_str()) {
        Confirm::Ok
    } else if EDIT_WORDS.contains(&p.as_str()) {
        Confirm::Edit
    } else {
        Confirm::Unknown
    }
}

// --------------------------------------------------------------------------
// Public API
// --------------------------------------------------------------------------

/// Start (or restart) the form. Sets step = "order_items".
pub fn start_form(app: &App, psid: &str) -> Reply {
    if let Err(err) = app.db.set_form(psid, "order_items", &HashMap::new(), 0) {
        error!(error = %err, "could not start order form");
    }
    question_for("order_items")
}

/// Re-ask the question for the stored step (used for stickers/images).
pub fn current_question(row: &FormRow) -> Reply {
    question_for(&row.step)
}

/// Advance the form with one input. Returns None when there is no active order form.
pub async fn handle_input(app: &App, psid: &str, raw: &str) -> Option<Reply> {
    let row = app.db.get_form(psid).ok().flatten()?;
    if kind_of_step(&row.step) != Some(FormKind::Order) {
        return None;
    }

    if is_cancel(raw) {
        let _ = app.db.delete_form(psid);
        return Some(Reply::text(Q_FORM_CANCELLED));
    }

    if row.step == "order_confirm" {
        return Some(handle_confirm(app, psid, &row, raw));
    }

    let Some(value) = validate(&row.step, raw) else {
        return Some(reask(app, psid, &row));
    };

    let key = row.step.strip_prefix("order_").unwrap_or(&row.step);
    let mut data = row.data.clone();
    data.insert(key.to_string(), value);

    match row.step.as_str() {
        "order_items" => Some(ask_confirm(app, psid, data).await),
        "order_contact" => Some(finish(app, psid, &data).await),
        step => {
            let next = if step == "order_name" { "order_address" } else { "order_contact" };
            if let Err(err) = app.db.set_form(psid, next, &data, 0) {
                error!(error = %err, "could not save order form step");
            }
            Some(question_for(next))
        }
    }
}

/// True when the input is an explicit "yes" for the receipt (ok / sige / opo / …).
pub fn is_confirm_yes(raw: &str) -> bool {
    confirm_intent(raw) == Confirm::Ok
}

/// Gentle reminder of the next step, used when the customer sends an acknowledgement while the
/// order form is still open.
pub fn step_hint(step: &str) -> &'static str {
    match step {
        "order_items" => "Para sa order, isulat lang po ang items 👇",
        "order_confirm" => "Para sa order, i-tap lang po ang 'Tama po' button 👇",
        "order_name" => "Para sa order, isulat lang po ang pangalan 👇",
        "order_address" => "Para sa order, isulat lang po ang address 👇",
        "order_contact" => "Para sa order, isulat lang po ang mobile number 👇",
        _ => "Para sa order, i-continue lang po natin ang form 👇",
    }
}

// --------------------------------------------------------------------------
// Internals
// --------------------------------------------------------------------------

fn validate(step: &str, raw: &str) -> Option<String> {
    match step {
        "order_items" => clean_len(raw, 2, 500),
        "order_name" => clean_name(raw),
        "order_address" => clean_len(raw, 5, 300),
        "order_contact" => clean_ph_mobile(raw),
        _ => None,
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
        "order_items" => Q_ORDER_ITEMS_HINT,
        "order_confirm" => Q_ORDER_CONFIRM_HINT,
        "order_name" => Q_ORDER_NAME_HINT,
        "order_address" => Q_ORDER_ADDRESS_HINT,
        _ => Q_ORDER_CONTACT_HINT,
    };
    let mut reply = Reply::text(format!("{Q_REASK_PREFIX}{hint}"));
    if row.step == "order_confirm" {
        reply.quick_replies = confirm_buttons();
    }
    reply
}

fn confirm_buttons() -> Vec<(String, String)> {
    vec![qr("Tama po ✅", PAYLOAD_OK), qr("Papalitan ✏️", PAYLOAD_EDIT)]
}

/// Show the priced receipt and ask the customer to confirm it.
async fn ask_confirm(app: &App, psid: &str, mut data: HashMap<String, String>) -> Reply {
    let raw_items = data.get("items").cloned().unwrap_or_default();
    let receipt = app.menu.build_receipt(&raw_items).await;

    data.insert("menu_note".into(), receipt.lines.join("\n"));
    if receipt.has_prices {
        data.insert("total_estimate".into(), fmt_price(receipt.total));
    } else {
        data.remove("total_estimate");
    }
    if let Err(err) = app.db.set_form(psid, "order_confirm", &data, 0) {
        error!(error = %err, "could not save order receipt");
    }

    let lines = receipt.lines.iter().map(|l| format!("• {l}")).collect::<Vec<_>>().join("\n");
    let total = if receipt.has_prices {
        fill(Q_TOTAL_ESTIMATE, &[("total", &fmt_price(receipt.total))])
    } else {
        Q_TOTAL_NO_PRICES.to_string()
    };

    let mut parts = vec!["Ito po ang order natin 🧾".to_string(), lines];
    parts.extend(receipt.alternates);
    parts.push(total);
    parts.push("Tama po ba ito, o may papalit?".to_string());
    Reply::new(parts.join("\n"), confirm_buttons())
}

/// Confirm step: accept / edit / neither.
fn handle_confirm(app: &App, psid: &str, row: &FormRow, raw: &str) -> Reply {
    match confirm_intent(raw) {
        Confirm::Ok => {
            // menu_note and total_estimate were stored by ask_confirm.
            if let Err(err) = app.db.set_form(psid, "order_name", &row.data, 0) {
                error!(error = %err, "could not advance order form");
            }
            question_for("order_name")
        }
        Confirm::Edit => {
            let mut data = row.data.clone();
            data.remove("menu_note");
            data.remove("total_estimate");
            if let Err(err) = app.db.set_form(psid, "order_items", &data, 0) {
                error!(error = %err, "could not restart items step");
            }
            info!("order receipt: customer edits items");
            question_for("order_items")
        }
        Confirm::Unknown => reask(app, psid, row),
    }
}

/// Save the order, notify the team, clear the form.
async fn finish(app: &App, psid: &str, data: &HashMap<String, String>) -> Reply {
    let get = |k: &str| data.get(k).cloned().unwrap_or_default();
    let (name, address, contact, estimate) = (get("name"), get("address"), get("contact"), get("total_estimate"));
    let items = match get("menu_note") {
        note if !note.is_empty() => note,
        _ => get("items"),
    };

    let order_id = match app.db.add_order(psid, &items, &name, &address, &contact, &estimate) {
        Ok(id) => id,
        Err(err) => {
            error!(error = %err, "could not save order");
            0
        }
    };
    let _ = app.db.delete_form(psid);

    let notifier = app.notifier.clone();
    let (p, i, e, n, a, c) = (psid.to_string(), items.clone(), estimate.clone(), name.clone(), address.clone(), contact.clone());
    tokio::spawn(async move {
        notifier.notify_order(&p, order_id, &i, &e, &n, &a, &c).await;
    });

    let total_line = if estimate.is_empty() {
        String::new()
    } else {
        format!("• Estimated total: ₱{estimate}\n")
    };
    Reply::text(fill(
        Q_FORM_DONE,
        &[
            ("name", &name),
            ("oid", &order_id.to_string()),
            ("items", &items),
            ("total_line", &total_line),
            ("address", &address),
            ("contact", &contact),
        ],
    ))
}

fn question_for(step: &str) -> Reply {
    match step {
        "order_items" => Reply::text(Q_ORDER_ITEMS),
        "order_name" => Reply::text(Q_ORDER_NAME),
        "order_address" => Reply::text(Q_ORDER_ADDRESS),
        "order_contact" => Reply::text(Q_ORDER_CONTACT),
        "order_confirm" => Reply::new(
            format!("{Q_ORDER_CONFIRM_HINT}"),
            confirm_buttons(),
        ),
        _ => Reply::text(Q_FORM_CANCELLED),
    }
}

// --------------------------------------------------------------------------
// Texts (Taglish)
// --------------------------------------------------------------------------

const Q_REASK_PREFIX: &str = "Pasensya po — ";

const Q_ORDER_ITEMS: &str = "Sige po! Order tayo 🛍️ Anong items po natin?\n\
(e.g. 'iced white choco mocha + 2 cookies' — isulat na lang po ang buong order nyo.)\n\
Paalala po: FREE po ang delivery sa Masaito at Lancaster residence; sa labas po natin isine-send via Lalamove. 😊";
const Q_ORDER_ITEMS_HINT: &str = "isulat po ang buong order ninyo, e.g. '2 pcs Iced Latte + 1 Blueberry Cheesecake'. \
('cancel' kung gusto nyo po tumigil)";
const Q_ORDER_NAME: &str = "Anong pangalan po natin for the order?";
const Q_ORDER_NAME_HINT: &str = "paki-type lang po ang pangalan (o pangalan ng nag-order), e.g. 'Maria Santos'. 😊";
const Q_ORDER_ADDRESS: &str = "Anong full address po (house/unit + street + subdivision/barangay)?\n\
🆓 FREE po natin: Masaito at Lancaster residence.";
const Q_ORDER_ADDRESS_HINT: &str = "paki-type po ang buong delivery address, e.g. 'Blk 2 Lot 3, Masaito Residence, Kawit'. \
('cancel' kung gusto nyo po tumigil)";
const Q_ORDER_CONTACT: &str = "Anong mobile number po? (para po sa delivery updates, e.g. 0917-123-4567 o +639171234567)";
const Q_ORDER_CONTACT_HINT: &str = "number po ba? Pakitype po ng PH mobile number, e.g. 09171234567. \
('cancel' kung ayaw nyo na po)";
const Q_ORDER_CONFIRM_HINT: &str = "Tama po ba ang order ninyo? Tap po ang button, o reply ng 'tama' \
o 'palit' kung may papalitan. ('cancel' para tumigil)";
const Q_TOTAL_ESTIMATE: &str = "Estimated total: ₱{total} (kakumpirmahin po ng team ang exact total)";
const Q_TOTAL_NO_PRICES: &str = "(Presyo po — kakumpirmahin ng team ang exact total)";

const Q_FORM_DONE: &str = "Salamat po, {name}! 🎉 Na-record na po ang order ninyo #{oid}:\n\
• Items:\n{items}\n\
{total_line}\
• Address: {address}\n\
• Mobile: {contact}\n\
• Delivery: FREE po ito sa Masaito/Lancaster residence — sa labas po, Lalamove natin at sasabihin ng team ang exact fee. 😊\n\n\
Ang aming team po ang magco-confirm DITO sa chat ng total, payment (cash/GCash) at delivery schedule. Sandali lang po sila. ☕";

const Q_FORM_CANCELLED: &str = "Sige po — kinansel ko po muna ang order form 😊 \
I-type lang po ang 'order' ulit kapag ready na kayo, o mag-message dito para makita ng team natin ang message nyo po.";
const Q_FORM_GAVE_UP: &str = "Hindi ko na po maituloy ang order form, kinansel ko po muna ito 😅 \
— i-type po ang 'order' ulit kahit kailan, o tap 'Order' button kapag ready.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_words() {
        assert_eq!(confirm_intent("tama po"), Confirm::Ok);
        assert_eq!(confirm_intent("Tama po ✅"), Confirm::Ok);
        assert_eq!(confirm_intent(PAYLOAD_OK), Confirm::Ok);
        assert_eq!(confirm_intent("edit"), Confirm::Edit);
        assert_eq!(confirm_intent("Mali!"), Confirm::Edit);
        assert_eq!(confirm_intent(PAYLOAD_EDIT), Confirm::Edit);
        assert_eq!(confirm_intent("random"), Confirm::Unknown);
    }

    #[test]
    fn validators() {
        assert_eq!(validate("order_items", "2 iced latte").as_deref(), Some("2 iced latte"));
        assert_eq!(validate("order_items", "x"), None);
        assert_eq!(validate("order_name", "Maria Santos").as_deref(), Some("Maria Santos"));
        assert_eq!(validate("order_address", "abc"), None);
        assert_eq!(validate("order_contact", "+63 917 123 4567").as_deref(), Some("09171234567"));
    }
}
