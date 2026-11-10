"""Order intake form — a deterministic state machine (NOT LLM-driven).

Steps: order_items -> order_name -> order_address -> order_contact -> done.
State lives in the same SQLite form slot as the event form (per PSID) — the
pipeline dispatches to this module whenever the active step starts with
"order_". The customer may type "cancel" (or similar) at any time.

The completed order is saved to the `orders` table and the team is notified
via Telegram. After the items step the bot shows a priced RECEIPT built from
the live POS menu (menu.py): real prices, real variant sizes; the displayed
total is an ESTIMATE — the team still confirms the final total in chat.
Payment methods and delivery schedules are NEVER confirmed by the bot.

Delivery policy: FREE within Masaito and Lancaster residence; anywhere else
is sent via Lalamove and the team quotes the exact fee before payment.
"""
from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Tuple

import db
import menu
import notify

from events import PHONE_CLEAN_RE

log = logging.getLogger("orders")

CANCEL_SET = {
    "cancel", "cancelled", "canceled", "cancel po", "cancel nl", "cancel it",
    "cancel order", "ayoko na", "nevermind", "never mind", "wag na",
    "wag nalang", "not now", "next time",
}

MAX_TRIES = 3

FREE_ZONES = "Masaito at Lancaster residence"


@dataclass
class Reply:
    text: str
    quick_replies: List[Tuple[str, str]] = field(default_factory=list)


# --------------------------------------------------------------------------
# Question texts (Taglish)
# --------------------------------------------------------------------------
Q_ORDER_ITEMS = (
    "Sige po! Order tayo 🛍️ Anong items po natin?\n"
    "(e.g. 'iced white choco mocha + 2 cookies' — isulat na lang po ang buong "
    "order nyo.)\n"
    "Paalala po: FREE po ang delivery sa Masaito at Lancaster residence; sa "
    "labas po natin isine-send via Lalamove. 😊"
)
Q_ORDER_ITEMS_HINT = (
    "Isulat po ang buong order ninyo, e.g. '2 pcs Iced Latte + 1 Blueberry "
    "Cheesecake'. ('cancel' kung gusto nyo po tumigil)"
)
Q_ORDER_NAME = "Anong pangalan po natin for the order?"
Q_ORDER_NAME_HINT = (
    "Paki-type lang po ang pangalan (o pangalan ng nag-order), e.g. 'Maria Santos'. 😊"
)
Q_ORDER_ADDRESS = (
    "Anong full address po (house/unit + street + subdivision/barangay)?\n"
    f"🆓 FREE po natin: {FREE_ZONES}."
)
Q_ORDER_ADDRESS_HINT = (
    "Paki-type po ang buong delivery address po, e.g. 'Blk 2 Lot 3, Masaito "
    "Residence, Kawit'. ('cancel' kung gusto nyo po tumigil)"
)
Q_ORDER_CONTACT = (
    "Anong mobile number po? (para po sa delivery updates, e.g. 0917-123-4567 "
    "o +639171234567)"
)
Q_ORDER_CONTACT_HINT = (
    "Pasensya po — number po ba? Pakitype po ng PH mobile number, e.g. "
    "09171234567. ('cancel' kung ayaw nyo na po)"
)
Q_ORDER_CONFIRM = (
    "Ito po ang order natin 🧾\n"
    "{lines}\n"
    "{alternates}"
    "{total}\n"
    "Tama po ba ito, o may papalit?"
)
Q_ORDER_CONFIRM_HINT = (
    "Tama po ba ang order ninyo? Reply po ng 'tama' kung oo, o 'mali/palit' "
    "kung may papalit. ('cancel' para tumigil)"
)
Q_TOTAL_ESTIMATE = "Estimated total: ₱{total} (kakumpirmahin pa po ng team ang exact total)"
Q_TOTAL_NO_PRICES = "(Presyo po — kakumpirmahin ng team ang exact total)"
Q_FORM_DONE = (
    "Salamat po, {name}! 🎉 Na-record na po ang order ninyo #{oid}:\n"
    "• Items: {items}\n"
    "{total_line}"
    "• Address: {address}\n"
    "• Mobile: {contact}\n"
    "• Delivery: FREE po ito sa Masaito/Lancaster residence — sa labas po, "
    "Lalamove natin at sasabihin ng team ang exact fee. 😊\n\n"
    "Ang aming team po ang magco-confirm DITO sa chat ng total, payment "
    "(cash/GCash) at delivery schedule. Sandali lang po sila. ☕"
)
Q_FORM_CANCELLED = (
    "Sige po — kinansel ko po muna ang order form 😊 "
    "I-type lang po ang 'order' ulit kapag ready na kayo, o mag-message dito "
    "para makita ng team natin ang message nyo po."
)
Q_FORM_GAVE_UP = (
    "Hindi ko na po maituloy ang order form, kinansel ko po muna ito 😅 — "
    "i-type po ang 'order' ulit kahit kailan, o tap 'Order' button kapag ready."
)
Q_REASK_PREFIX = "Pasensya po — "

_VALIDATOR_ARGS = Dict[str, str]


# --------------------------------------------------------------------------
# Public API
# --------------------------------------------------------------------------
async def has_active_form(psid: str) -> bool:
    return (await db.get_form(psid)) is not None


async def current_question(psid: str) -> Optional[Reply]:
    """Re-ask the current step's question (e.g. for images / stickers)."""
    row = await db.get_form(psid)
    if row is None:
        return None
    step, _data, _tries = row
    return _question_for(step)


async def start_form(psid: str) -> Reply:
    await db.set_form(psid, "order_items", {}, 0)
    log.info("order form started", extra={"psid": psid})
    return Reply(Q_ORDER_ITEMS)


async def handle_input(psid: str, raw_value: str) -> Optional[Reply]:
    """Advance the form with one input. Returns None when there is no form."""
    row = await db.get_form(psid)
    if row is None:
        return None
    step, data, tries = row
    intent = _interpret(raw_value)

    if intent == "cancel":
        await db.delete_form(psid)
        log.info("order form cancelled", extra={"psid": psid, "step": step})
        return Reply(Q_FORM_CANCELLED)

    return await _next(psid, _State(step=step, data=data, tries=tries), intent)


# --------------------------------------------------------------------------
# Internals
# --------------------------------------------------------------------------
@dataclass
class _State:
    step: str
    data: _VALIDATOR_ARGS
    tries: int


def _question_for(step: str) -> Reply:
    if step == "order_items":
        return Reply(Q_ORDER_ITEMS)
    if step == "order_confirm":
        return Reply(Q_ORDER_CONFIRM.format(
            lines="", alternates="", total=Q_TOTAL_NO_PRICES,
        ))
    if step == "order_name":
        return Reply(Q_ORDER_NAME)
    if step == "order_address":
        return Reply(Q_ORDER_ADDRESS)
    if step == "order_contact":
        return Reply(Q_ORDER_CONTACT)
    return Reply(Q_FORM_CANCELLED)


def _interpret(value: str) -> str:
    """Normalize a raw input to: 'cancel' | plain text."""
    low = " ".join(value.strip().lower().split())
    if (
        low in CANCEL_SET
        or re.search(r"\bcancels?(led|ed)?\b", low)
        or re.search(r"\bayoko na\b", low)
        or re.search(r"\bwag na\b", low)
    ):
        return "cancel"
    return value.strip()


async def _next(psid: str, state: _State, value: str) -> Reply:
    """Validate `value` for the current step; advance, re-ask or give up."""
    if state.step == "order_confirm":
        # The confirm step uses accept/edit/nudge logic, not free text.
        verdict = _val_confirm(value)  # 'ok' | 'edit' | None
        if verdict is None:
            state.tries += 1
            if state.tries >= MAX_TRIES:
                await db.delete_form(psid)
                log.info("order form gave up", extra={"psid": psid, "step": state.step})
                return Reply(Q_FORM_GAVE_UP)
            await db.set_form(psid, state.step, state.data, state.tries)
            return Reply(Q_REASK_PREFIX + Q_ORDER_CONFIRM_HINT)
        if verdict == "edit":
            # Restart the items step (keep nothing else cleared) — receipt prices
            # and anything related will be rebuilt on the next confirm.
            state.data.pop("menu_note", None)
            state.data.pop("total_estimate", None)
            await db.set_form(psid, "order_items", state.data, 0)
            log.info("order receipt: customer edits items", extra={"psid": psid})
            return Reply(Q_ORDER_ITEMS)
        # ok -> continue the intake (name/address/mobile); menu_note and
        # total_estimate were stored during _ask_confirm.
        await db.set_form(psid, "order_name", state.data, 0)
        return _question_for("order_name")

    validate: Optional[Any] = _VALIDATORS.get(state.step)
    if validate is None:  # unknown step (should not happen) — drop the form
        await db.delete_form(psid)
        return Reply(Q_FORM_CANCELLED)

    parsed = validate(value)
    if parsed is None:
        state.tries += 1
        if state.tries >= MAX_TRIES:
            await db.delete_form(psid)
            log.info("order form gave up", extra={"psid": psid, "step": state.step})
            return Reply(Q_FORM_GAVE_UP)
        await db.set_form(psid, state.step, state.data, state.tries)
        return Reply(Q_REASK_PREFIX + _HINTS[state.step])

    key = state.step.replace("order_", "")  # order_items -> items, ...
    state.data[key] = parsed
    if state.step == "order_contact":
        return await _finish(psid, state.data)
    if state.step == "order_items":
        return await _ask_confirm(psid, state)
    nxt = {
        "order_name": "order_address",
        "order_address": "order_contact",
    }[state.step]
    await db.set_form(psid, nxt, state.data, 0)
    return _question_for(nxt)


async def _ask_confirm(psid: str, state: _State) -> Reply:
    """Show the priced receipt (live POS menu) and ask the customer whether
    it's correct. The displayed total is an estimate only."""
    receipt = await menu.build_receipt(state.data.get("items", ""))
    state.data["menu_note"] = "\n".join([*receipt.lines, *receipt.alternates])
    if receipt.has_prices:
        state.data["total_estimate"] = menu._fmt_price(receipt.total)
        total_line = Q_TOTAL_ESTIMATE.format(total=menu._fmt_price(receipt.total))
    else:
        state.data["total_estimate"] = ""
        total_line = Q_TOTAL_NO_PRICES
    await db.set_form(psid, "order_confirm", state.data, 0)
    log.info("order receipt shown", extra={"psid": psid, "lines": len(receipt.lines)})
    alt = "\n".join(receipt.alternates) + ("\n" if receipt.alternates else "")
    text = Q_ORDER_CONFIRM.format(
        lines="\n".join(f"• {line}" for line in receipt.lines),
        alternates=alt,
        total=total_line,
    )
    return Reply(
        text,
        quick_replies=[("Tama po ✅", "ORD_OK"), ("Papalitan ✏️", "ORD_EDIT")],
    )


async def _finish(psid: str, data: _VALIDATOR_ARGS) -> Reply:
    """Save the order, notify the team via Telegram, pause the bot for the
    customer so the team can confirm the final total/schedule in the chat."""
    menu_note = data.get("menu_note", "")
    total_estimate = data.get("total_estimate", "")
    order_id = await db.add_order(
        psid=psid,
        items=data.get("items", ""),
        name=data.get("name", ""),
        address=data.get("address", ""),
        contact=data.get("contact", ""),
        menu_note=menu_note,
        total_estimate=total_estimate,
    )
    await db.delete_form(psid)
    # The customer follows up with staff now — silence the bot until the team
    # replies (their page-side message auto-resumes the bot).
    await db.set_paused(psid, True)
    log.info(
        "order saved",
        extra={"psid": psid, "order_id": order_id, "customer": data.get("name")},
    )
    sent = await notify.notify_order(
        psid=psid,
        name=data.get("name", ""),
        items=data.get("items", ""),
        address=data.get("address", ""),
        contact=data.get("contact", ""),
        menu_note=menu_note,
        total_estimate=total_estimate,
    )
    if not sent:
        log.error("order: telegram notify failed", extra={"psid": psid, "order_id": order_id})
    total_line = f"• Estimated total: ₱{total_estimate}\n" if total_estimate else ""
    return Reply(
        Q_FORM_DONE.format(
            oid=order_id,
            name=data.get("name", ""),
            items=data.get("items", ""),
            address=data.get("address", ""),
            contact=data.get("contact", ""),
            total_line=total_line,
        )
    )


# --------------------------------------------------------------------------
# Validators — canonical value, or None when the input is invalid
# --------------------------------------------------------------------------
def _val_items(value: str) -> Optional[str]:
    v = " ".join(value.strip().split())
    return v if 2 <= len(v) <= 500 else None


def _val_name(value: str) -> Optional[str]:
    v = value.strip()
    return v if 2 <= len(v) <= 120 else None


def _val_address(value: str) -> Optional[str]:
    v = value.strip()
    return v if 5 <= len(v) <= 300 else None


def _val_contact(value: str) -> Optional[str]:
    """Lenient PH mobile — same rule as the event form."""
    digits = PHONE_CLEAN_RE.sub("", value.strip())
    digits = digits.lstrip("+")
    if re.fullmatch(r"(?:0|63)?9\d{9}", digits):
        return digits
    return None


_CONFIRM_YES = {
    "tama", "tama po", "tama na", "oo", "oo po", "opo", "yes", "yes po",
    "sige", "sige po", "ok", "ok po", "okay", "okay po", "correct",
    "totoo", "yes tama", "ayos", "ayos lang", "ayos lang po", "go", "sure",
    "tama na po", "tama lang po",
}
_CONFIRM_EDIT = {
    "edit", "edit po", "mali", "mali po", "palit", "palitan", "palitan po",
    "change", "change po", "uli", "ulit", "ulitin", "ulitin po", "replace",
    "replace po", "hindi", "hindi po", "iba", "iba po", "papalit",
    "papalitan", "papalitan po", "may papalit", "may papalit po",
}


def _val_confirm(value: str) -> Optional[str]:
    """'ok' when the customer accepts the receipt, 'edit' to redo items,
    None when the input is neither (counts toward the give-up limit)."""
    low = " ".join(value.strip().lower().split())
    if low in _CONFIRM_YES:
        return "ok"
    if low in _CONFIRM_EDIT:
        return "edit"
    # Some phrases containing the keywords still count (e.g. "tama po,
    # kasama ba ang syrup?").
    if re.search(r"\b(tama|sige|oo|opo)\b", low) and not re.search(
        r"\b(mali|palit|edit|iba|hindi)\b", low
    ):
        return "ok"
    if re.search(r"\b(mali|palit|edit|iba|papalitan|ulitin|replace)\b", low):
        return "edit"
    return None


_VALIDATORS: Dict[str, Any] = {
    "order_items": _val_items,
    "order_confirm": _val_confirm,
    "order_name": _val_name,
    "order_address": _val_address,
    "order_contact": _val_contact,
}

_HINTS: Dict[str, str] = {
    "order_items": Q_ORDER_ITEMS_HINT,
    "order_confirm": Q_ORDER_CONFIRM_HINT,
    "order_name": Q_ORDER_NAME_HINT,
    "order_address": Q_ORDER_ADDRESS_HINT,
    "order_contact": Q_ORDER_CONTACT_HINT,
}
