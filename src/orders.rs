//! Order intake form — a deterministic state machine (NOT LLM-driven).
//!
//! Steps: order_items -> order_name -> order_address -> order_contact -> done.
//! State lives in the same SQLite form slot as the event form (per PSID) — the
//! pipeline dispatches to this module whenever the active step starts with
//! "order_". The customer may type "cancel" (or similar) at any time.
//!
//! The completed order is saved to the `orders` table and the team is notified
//! via Telegram. After the items step the bot shows a priced RECEIPT built from
//! the live POS menu (menu.py): real prices, real variant sizes; the displayed
//! total is an ESTIMATE — the team still confirms the final total in chat.
//! Payment methods and delivery schedules are NEVER confirmed by the bot.

use crate::config::Settings;
use crate::db::Db;
use crate::events::Q_FORM_DONE;
use crate::menu;
use crate::messenger::Messenger;
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

const CANCEL_SET: &[&str] = &[
    "cancel", "cancelled", "canceled", "cancel po", "cancel nl", "cancel it",
    "cancel order", "ayoko na", "nevermind", "never mind", "wag na",
    "wag nalang", "not now", "next time",
];

/// Map of yes/edit confirm answers
#[derive(Clone, Debug)]
pub struct ConfirmMap(HashMap<String, String>);

impl ConfirmMap {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert("tama".into(), "ok".into());
        map.insert("tama po".into(), "ok".into());
        map.insert("tama na".into(), "ok".into());
        map.insert("oo".into(), "ok".into());
        map.insert("oo po".into(), "ok".into());
        map.insert("opo".into(), "ok".into());
        map.insert("yes".into(), "ok".into());
        map.insert("yes po".into(), "ok".into());
        map.insert("sige".into(), "ok".into());
        map.insert("sige po".into(), "ok".into());
        map.insert("ok".into(), "ok".into());
        map.insert("ok po".into(), "ok".into());
        map.insert("okay".into(), "ok".into());
        map.insert("okay po".into(), "ok".into());
        map.insert("correct".into(), "ok".into());
        map.insert("ayos".into(), "ok".into());
        map.insert("ayos lang".into(), "ok".into());
        map.insert("ayos lang po".into(), "ok".into());
        map.insert("go".into(), "ok".into());
        map.insert("sure".into(), "ok".into());
        ConfirmMap(map)
    }

    pub fn lookup(&self, value: &str) -> Option<&str> {
        self.0.get(value).map(|s| s.as_str())
    }
}

/// Pre-built instance so bot code stays the same as Python.
pub static CONFIRM_MAP: ConfirmMap = ConfirmMap::new();

/// ---------------------------------------------------------------------
//  Public API
//  ---------------------------------------------------------------------

/// Return true if the customer currently has an active order form.
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

/// Start the form from scratch. Sets step = "order_items".
pub async fn start_form(db: &Db, psid: &str) -> Reply {
    db.set_form(psid, "order_items", &HashMap::new(), 0)
        .await
        .ok();
    Reply::new(Q_ORDER_ITEMS.clone(), vec![])
}

/// Advance the form with one input. Returns None when there is no form.
pub async fn handle_input(
    messenger: &Arc<Messenger>,
    db: &Db,
    menu_mod: &menu::Menu,
    psid: &str,
    raw_value: &str,
) -> Option<Reply> {
    let row = db.get_form(psid).await.ok()?;
    let (step, data, tries) = (row.step.clone(), row.data.clone(), row.tries);

    // Cancel?
    if is_cancel(raw_value) {
        db.delete_form(psid).await.ok()?;
        return Some(Reply::new(Q_FORM_CANCELLED.clone(), vec![]));
    }

    // Interpret: what did the customer say?
    let low = raw_value.to_lowercase().trim().to_string();

    // Confirm step?
    if step == "order_confirm" {
        return handle_confirm(messenger, db, menu_mod, psid, &low).await;
    }

    // Normal steps validation
    let parsed = match step.as_str() {
        "order_items" => _val_items(&low),
        "order_name" => _val_name(&low),
        "order_address" => _val_address(&low),
        "order_contact" => _val_contact(&low),
        _ => None,
    };

    let Some(parsed) = parsed else {
        // re-ask
        let new_tries = if tries >= 3 { 0 } else { tries + 1 };
        db.set_form(psid, &step, &data, new_tries).await.ok()?;
        let hint = _HINTS.get(step).map(|h| h.clone()).unwrap_or_default();
        return Some(Reply::new(format!("{}{}REASK_PREFIX", Q_REASK_PREFIX, hint), vec![]));
    };

    // Store the parsed value
    let key = step.strip_prefix("order_").unwrap_or(step);
    let mut new_data = data.clone();
    new_data.insert(key.into(), parsed.clone());

    // If we just finished items, ask for confirmation (with receipt)
    if step == "order_items" {
        return await ask_confirm(messenger, db, menu_mod, psid, &new_data).await;
    }

    // Otherwise advance to next step
    let nxt = match step.as_str() {
        "order_name" => "order_address",
        "order_address" => "order_contact",
        _ => "order_items",
    };
    db.set_form(psid, &nxt, &new_data, 0)
        .await
        .ok();
    Some(_question_for_order(&nxt))
}

/// Check if input is a cancel command
fn is_cancel(value: &str) -> bool {
    let s = value.trim().to_lowercase();
    CANCEL_SET.iter().any(|&kw| s == kw || s.starts_with(kw) && s.len() > kw.len())
}

/// Ask the customer to confirm the receipt (with live prices).
async fn ask_confirm(
    messenger: &Arc<Messenger>,
    db: &Db,
    menu_mod: &menu::Menu,
    psid: &str,
    data: &HashMap<String, String>,
) -> Option<Reply> {
    let raw_items = data.get("items")?.clone().unwrap_or_default();
    let receipt = menu_mod.build_receipt(&raw_items).await;
    let mut new_data = data.clone();
    new_data.insert("menu_note".into(), receipt.lines.join("\n"));
    if receipt.has_prices {
        new_data.insert(
            "total_estimate".into(),
            menu::_fmt_price(receipt.total),
        );
    } else {
        new_data.remove("total_estimate");
    }
    db.set_form(psid, "order_confirm", &new_data, 0)
        .await
        .ok();

    let lines: String = receipt
        .lines
        .iter()
        .map(|l| format!("• {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let alt = if !receipt.alternates.is_empty() {
        format!("\n{}", receipt.alternates.join("\n"))
    } else {
        String::new()
    };
    let total = if !receipt.has_prices {
        Q_TOTAL_NO_PRICES.into()
    } else {
        Q_TOTAL_ESTIMATE.format(total = menu::_fmt_price(receipt.total))
    };

    let text = Q_ORDER_CONFIRM.format(
        lines = if lines.is_empty() { "" } else { &lines },
        alternates = &alt,
        total = &total,
    );

    let quick = if receipt.has_prices {
        vec![
            ("Tama po ✅", "ORD_OK"),
            ("Papalatan ✏️", "ORD_EDIT"),
        ]
    } else {
        vec![]
    };

    Some(Reply {
        text,
        quick_replies: quick,
    })
}

/// Handle the confirm step: accept / edit / neither
async fn handle_confirm(
    messenger: &Arc<Messenger>,
    db: &Db,
    menu_mod: &menu::Menu,
    psid: &str,
    low: &str,
) -> Option<Reply> {
    let row = db.get_form(psid).await.ok()?;
    let (step, data, tries) = (row.step.clone(), row.data.clone(), row.tries);

    // Look up confirm intent
    let verdict = CONFIRM_MAP.lookup(low)?;

    match verdict {
        "ok" => {
            // Continue the intake (name/address/mobile); menu_note and
            // total_estimate were stored during _ask_confirm.
            db.set_form(psid, "order_name", &data, 0)
                .await
                .ok();
            Some(_question_for_order("order_name"))
        }
        "edit" => {
            // Restart the items step (keep nothing else cleared)
            let mut new_data = data.clone();
            new_data.remove("menu_note");
            new_data.remove("total_estimate");
            db.set_form(psid, "order_items", &new_data, 0)
                .await
                .ok();
            log::info!("order receipt: customer edits items");
            Some(_question_for_order("order_items"))
        }
        _ => {
            // Neither: count toward give-up
            let new_tries = if tries >= 3 { 0 } else { tries + 1 };
            db.set_form(psid, &step, &data, new_tries).await.ok()?;
            Some(Reply::new(format!("{}{}REASK_PREFIX", Q_REASK_PREFIX, Q_ORDER_CONFIRM_HINT), vec![]))
        }
    }
}

/// Validators

fn _val_items(value: &str) -> Option<String> {
    let v = value.trim();
    if 2 <= v.len() && v.len() <= 500 {
        return Some(v.into());
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

fn _val_address(value: &str) -> Option<String> {
    let v = value.trim();
    if 5 <= v.len() && v.len() <= 300 {
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

/// Question text mapping
fn _question_for_order(step: &str) -> Reply {
    match step {
        "order_items" => Reply::new(Q_ORDER_ITEMS.clone(), vec![]),
        "order_name" => Reply::new(Q_ORDER_NAME.clone(), vec![]),
        "order_address" => Reply::new(Q_ORDER_ADDRESS.clone(), vec![]),
        "order_contact" => Reply::new(Q_ORDER_CONTACT.clone(), vec![]),
        "order_confirm" => Reply::new(Q_ORDER_CONFIRM.format(
            lines = "",
            alternates = "",
            total = Q_TOTAL_NO_PRICES.into(),
        ), vec![]),
        _ => Reply::new(Q_FORM_CANCELLED.into(), vec![]),
    }
}

// Global question texts
static Q_ORDER_ITEMS: &str = "Sige po! Order tayo 🛍️ Anong items po natin?\n\
(e.g. 'iced white choco mocha + 2 cookies' — isulat na lang po ang buong \
order nyo.)\n\
Paalala po: FREE po ang delivery sa Masaito at Lancaster residence; sa \
labas po natin isine-send via Lalamove. 😊";

static Q_ORDER_ITEMS_HINT: &str = "Isulat po ang buong order ninyo, e.g. '2 pcs Iced Latte + 1 Blueberry \
Cheesecake'. ('cancel' kung gusto nyo po tumigil)";

static Q_ORDER_NAME: &str = "Anong pangalan po natin for the order?";

static Q_ORDER_NAME_HINT: &str = "Paki-type lang po ang pangalan (o pangalan ng nag-order), e.g. 'Maria Santos'. 😊";

static Q_ORDER_ADDRESS: &str = "Anong full address po (house/unit + street + subdivision/barangay)?\n\
🆓 FREE po natin: Masaito at Lancaster residence.";

static Q_ORDER_ADDRESS_HINT: &str = "Paki-type po ang buong delivery address po, e.g. 'Blk 2 Lot 3, \
Masaito Residence, Kawit'. ('cancel' kung gusto nyo po tumigil)";

static Q_ORDER_CONTACT: &str = "Anong mobile number po? (para po sa delivery updates, e.g. 0917-123-4567 \
o +639171234567)";

static Q_ORDER_CONTACT_HINT: &str = "Pasensya po — number po ba? Pakitype po ng PH mobile number, e.g. \
09171234567. ('cancel' kung ayaw nyo na po)";

static Q_ORDER_CONFIRM: &str = "Ito po ang order natin 🧾\n\
{lines}\n\
{alternates}\n\
{total}\n\
Tama po ba ito, o may papalit?";

static Q_ORDER_CONFIRM_HINT: &str = "Tama po ba ang order ninyo? Reply po ng 'tama' \
po ba o 'mali/palit' kung may papalit. ('cancel' para tumigil)";

static Q_TOTAL_ESTIMATE: &str = "Estimated total: ₱{total} (kakumpirmahin po ng team ang exact total)";

static Q_TOTAL_NO_PRICES: &str = "(Presyo po — kakumpirmahin ng team ang exact total)";

static Q_FORM_DONE: &str = "Salamat po, {name}! 🎉 Na-record na po ang order ninyo #{oid}:\n\
• Items: {items}\n\
{total_line}\n\
• Address: {address}\n\
• Mobile: {contact}\n\
• Delivery: FREE po ito sa Masaito/Lancaster residence — sa labas po, \
Lalamove natin at sasabihin ng team ang exact fee. 😊\n\n\
Ang aming team po ang magco-confirm DITO sa chat ng total, payment \
(cash/GCash) at delivery schedule. Sandali lang po sila. ☕";

static Q_FORM_CANCELLED: &str = "Sige po — kinansel ko po muna ang order form 😊 \
I-type lang po ang 'order' ulit kapag ready na kayo, o mag-message dito \
para makita ng team natin ang message nyo po.";

static Q_FORM_GAVE_UP: &str = "Hindi ko na po maituloy ang order form, kinansel ko po muna ito 😅 \
— i-type po ang 'order' ulit kahit kailan, o tap 'Order' button kapag ready.";

static Q_REASK_PREFIX: &str = "Pasensya po — ";

// -HINTS
static _HINTS: &[(&str, &str)] = &[
    ("order_items", &Q_ORDER_ITEMS_HINT),
    ("order_confirm", &Q_ORDER_CONFIRM_HINT),
    ("order_name", &Q_ORDER_NAME_HINT),
    ("order_address", &Q_ORDER_ADDRESS_HINT),
    ("order_contact", &Q_ORDER_CONTACT_HINT),
];

// --------------------------------------------------------------------------
// Q_REASK_PREFIX
// --------------------------------------------------------------------------

const Q_REASK_PREFIX: &str = "Pasensya po — ";

// --------------------------------------------------------------------------
// DEBUG helper
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::menu::Menu;
    use crate::config::Settings;
    use std::collections::HashMap;

    #[test]
    fn test_cancel_detection() {
        assert!(is_cancel("cancel"));
        assert!(is_cancel("Cancel po"));
        assert!(is_cancel("wag na"));
        assert!(!is_cancel("2 iced latte"));
    }

    #[test]
    fn test_val_items() {
        assert_eq!(_val_items("2 iced latte"), Some("2 iced latte".into()));
        assert_eq!(_val_items("x"), None);
    }

    #[test]
    fn test_confirm_map() {
        assert_eq!(CONFIRM_MAP.lookup("tama po"), Some("ok"));
        assert_eq!(CONFIRM_MAP.lookup("edit"), Some("edit"));
        assert_eq!(CONFIRM_MAP.lookup("mali"), Some("edit"));
        assert_eq!(CONFIRM_MAP.lookup("random"), None);
    }
}