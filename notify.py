"""Telegram notifications for the owner/team: new event leads, handoffs,
LLM failures. Fire-and-forget with hard error swallowing; no secrets ever
included in messages or logs.
"""
from __future__ import annotations

import logging
from typing import Any, Dict, Optional

import httpx

import config

log = logging.getLogger("notify")

_API = "https://api.telegram.org"
_TIMEOUT = 10.0


def _configured() -> bool:
    return bool(config.settings.telegram_bot_token and config.settings.telegram_chat_id)


async def send_telegram(text: str, client: Optional[httpx.AsyncClient] = None) -> bool:
    """Send a message to the configured chat. Returns False on any failure."""
    if not _configured():
        log.debug("telegram not configured; notification skipped")
        return False
    url = f"{_API}/bot{config.settings.telegram_bot_token}/sendMessage"
    payload: Dict[str, Any] = {
        "chat_id": config.settings.telegram_chat_id,
        "text": text[:4000],
        "disable_notification": False,
    }
    try:
        if client is None:
            async with httpx.AsyncClient(timeout=_TIMEOUT) as tmp:
                resp = await tmp.post(url, json=payload)
        else:
            resp = await client.post(url, json=payload)
    except (httpx.HTTPError, OSError) as exc:
        log.error("telegram send failed", extra={"error": type(exc).__name__})
        return False
    if resp.status_code >= 400:
        log.error("telegram send failed", extra={"status": resp.status_code})
        return False
    return True


def _snippet(text: str, limit: int = 200) -> str:
    text = (text or "").strip().replace("\n", " ")
    return text[:limit] + ("…" if len(text) > limit else "")


async def notify_event_lead(
    psid: str,
    event_type: str,
    event_when: str,
    guests: int,
    name: str,
    contact: str,
) -> bool:
    text = (
        "🎉 NEW EVENT LEAD — Erlbrew Café\n"
        f"• Name: {name}\n"
        f"• Type: {event_type}\n"
        f"• Date/time: {event_when}\n"
        f"• Guests: {guests}\n"
        f"• Contact: {contact}\n"
        f"• PSID: {psid}\n"
        "Reply to the guest on Facebook this day. Availability is NOT confirmed yet."
    )
    return await send_telegram(text)


async def notify_order(
    psid: str,
    *,
    name: str,
    items: str,
    address: str,
    contact: str,
    menu_note: str = "",
    total_estimate: str = "",
) -> bool:
    receipt = ""
    if menu_note:
        receipt = f"🧾 Receipt (estimate from POS):\n{menu_note}\n"
        if total_estimate:
            receipt += f"• Estimated total: ₱{total_estimate}\n"
    text = (
        "🛍️ NEW ORDER — Erlbrew Café\n"
        f"• Name: {name}\n"
        f"• Items: {_snippet(items, 300)}\n"
        f"{receipt}"
        f"• Address: {_snippet(address, 200)}\n"
        f"• Contact: {contact}\n"
        f"• PSID: {psid}\n"
        "Estimates ONLY — confirm the final total, payment (cash/GCash) and "
        "delivery schedule with the customer IN MESSENGER. Bot replies are "
        "paused for them until you reply."
    )
    return await send_telegram(text)


async def notify_handoff(psid: str, name: str, message: str) -> bool:
    display = name or "(unknown name)"
    text = (
        "🤝 HANDOFF — customer needs a human (Erlbrew Café)\n"
        f"• Name: {display}\n"
        f"• PSID: {psid}\n"
        f"• Message: {_snippet(message)}"
    )
    return await send_telegram(text)


async def notify_llm_failure(psid: str, reason: str) -> bool:
    text = (
        "⚠️ LLM FALLBACK FAILED — Erlbrew Café\n"
        f"• PSID: {psid}\n"
        f"• Reason: {_snippet(reason, 120)}\n"
        "Static fallback message was sent to the customer."
    )
    return await send_telegram(text)
