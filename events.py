"""Event booking form — a deterministic state machine (NOT LLM-driven).

Steps: event_type -> event_when -> guests -> name -> contact -> done.
State lives in SQLite (per PSID). The customer may type "cancel" (or similar)
at any time. After completion the lead is saved and the team is notified via
Telegram; availability, dates and prices are NEVER confirmed by the bot.
"""
from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Tuple

import config
import db
import notify

log = logging.getLogger("events")

TYPE_PAYLOAD_PREFIX = "EVT_TYPE:"

EVENT_TYPES = {
    "birthday": "Birthday",
    "kaarawan": "Birthday",
    "meeting": "Meeting",
    "miting": "Meeting",
    "photoshoot": "Photoshoot",
    "photo shoot": "Photoshoot",
    "pictorial": "Photoshoot",
    "other": "Other",
    "iba": "Other",
}

CANCEL_SET = {
    "cancel", "cancelled", "canceled", "cancel po", "cancel nl", "cancel it",
    "ayoko na", "nevermind", "never mind", "wag na", "wag nalang",
    "cancel event", "cancel booking",
}

MAX_TRIES = 3
PHONE_CLEAN_RE = re.compile(r"[\s\-().]", re.UNICODE)


@dataclass
class Reply:
    text: str
    quick_replies: List[Tuple[str, str]] = field(default_factory=list)


# --------------------------------------------------------------------------
# Question texts (Taglish, [PLACEHOLDERS] for the owner)
# --------------------------------------------------------------------------
Q_EVENT_TYPE = (
    "[PLACEHOLDER: intro line kung gusto nyo — e.g. Sige po! Mag-book tayo ng event! 🎉] "
    "Anong klase po ng event?"
)
Q_EVENT_TYPE_HINT = (
    "Piliin lang po natin: Birthday 🎂, Meeting 🧾, Photoshoot 📸, o Other ✨. "
    "(Pwdi rin po kayo mag-type ng 'cancel' kahit anong oras para tumigil.)"
)
Q_EVENT_WHEN = (
    "Anong petsa at oras po? (e.g. 'May 12, 3 PM' o 'Sabado 9 AM')\n"
    "PAALALA po: hindi pa po ito confirmation ng schedule — "
    "ang team namin po ang tatawag mag-confirm ng availability. 😊"
)
Q_EVENT_WHEN_HINT = (
    "Paki-type po ang petsa at oras, e.g. 'May 12 3 PM' o 'Sabado 9 AM'. "
    "('cancel' kung gusto nyo po tumigil)"
)
Q_GUESTS = "Ilang guests po ang ine-expect nyo? (number lang po, e.g. 12)"
Q_GUESTS_HINT = "Paki-type lang po ang bilang ng guests as a number po, e.g. 12. 😊"
Q_NAME = "Sige po! Anong pangalan po natin for the booking?"
Q_NAME_HINT = "Paki-type lang po ang pangalan (o pangalan ng group), e.g. 'Maria Santos'. 😊"
Q_CONTACT = (
    "Anong mobile number po natin? (para po sa updates, e.g. 0917-123-4567 "
    "o +639171234567)"
)
Q_CONTACT_HINT = (
    "Pasensya po — number po ba? Pakitype po ng PH mobile number, e.g. 09171234567. "
    "('cancel' kung ayaw nyo na po)"
)
Q_FORM_DONE = (
    "Salamat po, {name}! 🎉 Naipasa na po namin ang booking nyo:"
    "\n• Type: {etype}"
    "\n• Petsa/oras: {when}"
    "\n• Guests: {guests}"
    "\n• Pangalan: {name}"
    "\n• Number: {contact}"
    "\n\nSasagot po ang aming team within {hours} oras dito sa Messenger. "
    "PAALALA po: hindi po ito confirmation ng availability, petsa o presyo — "
    "ang team po namin ang magsasabi kung available ang petsa at kung magkano. "
    "Salamat po! ☕"
)
Q_FORM_CANCELLED = (
    "Sige po — kinansel ko na po muna ang booking form. "
    "I-type lang po ang tanong nyo dito, o tap 'Menu' sa persistent menu.😊"
)
Q_FORM_GAVE_UP = (
    "Hindi ko na po maituloy ang form, kinansel ko po muna ito 😅 — "
    "i-type na lang po ang tanong nyo dito, o tap 'Events' ulit kapag ready na po kayo."
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
    step, data, _tries = row
    return _question_for(step)


async def start_form(psid: str) -> Reply:
    await db.set_form(psid, "event_type", {}, 0)
    log.info("event form started", extra={"psid": psid})
    return Reply(Q_EVENT_TYPE, quick_replies=_type_tiles())


async def handle_input(psid: str, raw_value: str) -> Optional[Reply]:
    """Advance the form with one input. Returns None when there is no form."""
    row = await db.get_form(psid)
    if row is None:
        return None
    step, data, tries = row
    intent = _interpret(raw_value)

    if intent == "cancel":
        await db.delete_form(psid)
        log.info("event form cancelled", extra={"psid": psid, "step": step})
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


def _type_tiles() -> List[Tuple[str, str]]:
    return [
        ("Birthday 🎂", f"{TYPE_PAYLOAD_PREFIX}Birthday"),
        ("Meeting 🧾", f"{TYPE_PAYLOAD_PREFIX}Meeting"),
        ("Photoshoot 📸", f"{TYPE_PAYLOAD_PREFIX}Photoshoot"),
        ("Other ✨", f"{TYPE_PAYLOAD_PREFIX}Other"),
    ]


def _question_for(step: str) -> Reply:
    if step == "event_type":
        return Reply(Q_EVENT_TYPE, quick_replies=_type_tiles())
    if step == "event_when":
        return Reply(Q_EVENT_WHEN)
    if step == "guests":
        return Reply(Q_GUESTS)
    if step == "name":
        return Reply(Q_NAME)
    if step == "contact":
        return Reply(Q_CONTACT)
    return Reply(Q_FORM_CANCELLED)


def _interpret(value: str) -> str:
    """Normalize a raw input to: 'cancel' | 'EVT_TYPE:<x>' | plain text."""
    low = re.sub(r"\s+", " ", value.strip().lower())
    if low in CANCEL_SET or re.search(r"\bcancels?(led|ed)?\b", low) or re.search(r"\bayoko na\b", low):
        return "cancel"
    if low.startswith(TYPE_PAYLOAD_PREFIX.lower()):
        return "EVT_TYPE:" + low[len(TYPE_PAYLOAD_PREFIX):].strip()
    return value.strip()


async def _next(psid: str, state: _State, value: str) -> Reply:
    """Validate `value` for the current step; advance, re-ask or give up."""
    validate: Optional[Callable[[str], Optional[str]]] = _VALIDATORS.get(state.step)
    if validate is None:  # unknown step (should not happen) — drop the form
        await db.delete_form(psid)
        return Reply(Q_FORM_CANCELLED)

    parsed = validate(value)
    if parsed is None:
        state.tries += 1
        if state.tries >= MAX_TRIES:
            await db.delete_form(psid)
            log.info("event form gave up", extra={"psid": psid, "step": state.step})
            return Reply(Q_FORM_GAVE_UP)
        await db.set_form(psid, state.step, state.data, state.tries)
        return Reply(Q_REASK_PREFIX + _HINTS[state.step])

    key = state.step.replace("event_", "")  # event_type -> type, event_when -> when
    state.data[key] = parsed
    if state.step == "contact":
        return await _finish(psid, state.data)
    nxt = {
        "event_type": "event_when",
        "event_when": "guests",
        "guests": "name",
        "name": "contact",
    }[state.step]
    await db.set_form(psid, nxt, state.data, 0)
    return _question_for(nxt)


async def _finish(psid: str, data: _VALIDATOR_ARGS) -> Reply:
    """Save the lead, notify the team via Telegram, clear the form."""
    guests = int(data.get("guests", "0"))
    lead_id = await db.add_lead(
        psid=psid,
        event_type=data.get("type", "Other"),
        event_when=data.get("when", ""),
        guests=guests,
        name=data.get("name", ""),
        contact=data.get("contact", ""),
    )
    await db.delete_form(psid)
    log.info("event lead saved", extra={"psid": psid, "lead_id": lead_id, "type": data.get("type")})
    sent = await notify.notify_event_lead(
        psid=psid,
        event_type=data.get("type", "Other"),
        event_when=data.get("when", ""),
        guests=guests,
        name=data.get("name", ""),
        contact=data.get("contact", ""),
    )
    if not sent:
        log.error("event lead: telegram notify failed", extra={"psid": psid, "lead_id": lead_id})
    return Reply(
        Q_FORM_DONE.format(
            name=data.get("name", ""),
            etype=data.get("type", "Other"),
            when=data.get("when", ""),
            guests=data.get("guests", "?"),
            contact=data.get("contact", ""),
            hours=config.settings.event_reply_hours,
        )
    )


# --------------------------------------------------------------------------
# Validators — canonical value, or None when the input is invalid
# --------------------------------------------------------------------------
def _val_type(value: str) -> Optional[str]:
    low = value.lower()
    if low.startswith(TYPE_PAYLOAD_PREFIX.lower()):
        low = low[len(TYPE_PAYLOAD_PREFIX):].strip()
    for needle, canonical in EVENT_TYPES.items():
        if re.search(rf"\b{re.escape(needle)}\b", low):
            return canonical
    return None


def _val_when(value: str) -> Optional[str]:
    v = value.strip()
    if 4 <= len(v) <= 120 and re.search(r"\d", v):
        return v
    return None


def _val_guests(value: str) -> Optional[str]:
    match = re.search(r"\d{1,4}", value.strip())
    if not match:
        return None
    n = int(match.group())
    return str(n) if 1 <= n <= 1000 else None


def _val_name(value: str) -> Optional[str]:
    v = value.strip()
    return v if 2 <= len(v) <= 120 else None


def _val_contact(value: str) -> Optional[str]:
    """Lenient PH mobile: 09XXXXXXXXX / 9XXXXXXXXX / +639XXXXXXXXX / 639XXXXXXXXX."""
    digits = PHONE_CLEAN_RE.sub("", value.strip())
    digits = digits.lstrip("+")
    if re.fullmatch(r"(?:0|63)?9\d{9}", digits):
        return digits
    return None


_VALIDATORS: Dict[str, Callable[[str], Optional[str]]] = {
    "event_type": _val_type,
    "event_when": _val_when,
    "guests": _val_guests,
    "name": _val_name,
    "contact": _val_contact,
}

_HINTS: Dict[str, str] = {
    "event_type": Q_EVENT_TYPE_HINT,
    "event_when": Q_EVENT_WHEN_HINT,
    "guests": Q_GUESTS_HINT,
    "name": Q_NAME_HINT,
    "contact": Q_CONTACT_HINT,
}
