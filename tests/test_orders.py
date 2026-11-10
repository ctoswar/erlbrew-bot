"""Order intake form: intent detection, steps, validation, cancel, rescue."""
from __future__ import annotations

import asyncio
import json

import menu
import pytest

import config
import db

from conftest import flush, make_message_event, post_message
from conftest import _webhook_body, sign_headers


async def _active_step(psid: str) -> str | None:
    row = await db.get_form(psid)
    return row[0] if row else None


_FAKE_MENU = [
    {"id": "H5", "name": "WHITE CHOCOLATE MOCHA", "category": "HOT COFFEE",
     "price": 129.0, "description": "8oz", "popular": 1},
    {"id": "M4", "name": "ICED WHITE CHOCOLATE MOCHA", "category": "ICED COFFEE",
     "price": 169.0, "description": "16oz", "popular": 0},
    {"id": "CS1", "name": "CHOCOLATE COOKIES", "category": "PASTRIES",
     "price": 85.0, "description": "", "popular": 0},
]


@pytest.fixture
def menu_rows(monkeypatch):
    """Inject a small POS menu (no MySQL involved)."""
    menu.reset_cache()

    async def fake_rows():
        return [dict(r) for r in _FAKE_MENU]

    monkeypatch.setattr(menu, "_fetch_rows", fake_rows)
    return _FAKE_MENU


async def test_order_intent_starts_form(api, sends, tg):
    psid = "PSID_ORD1"
    await post_message(api, make_message_event(psid=psid, text="pabili po ako ng latte"))
    await flush()
    assert await _active_step(psid) == "order_items"
    assert "order" in sends.texts()[-1].lower()
    # not paused yet — only after the form completes
    assert not await db.get_paused(psid)


async def test_order_intent_payload_order_start(api, sends, tg):
    psid = "PSID_ORD_PAYLOAD"
    await post_message(api, make_message_event(psid=psid, quick_payload="ORDER_START"))
    await flush()
    assert await _active_step(psid) == "order_items"


async def test_full_happy_flow_saves_order_and_pauses(api, sends, tg):
    psid = "PSID_ORD2"
    await post_message(api, make_message_event(psid=psid, quick_payload="ORDER_START"))
    await flush()

    await post_message(api, make_message_event(psid=psid, text="2 iced latte + 1 banana bread"))
    await flush()
    # POS set up but no menu rows configured in tests: confirm shown without prices
    assert await _active_step(psid) == "order_confirm"
    confirm = sends.texts()[-1]
    assert "tama po ba" in confirm.lower()
    assert "iced latte" in confirm.lower()
    await post_message(api, make_message_event(psid=psid, text="tama po"))
    await flush()
    assert await _active_step(psid) == "order_name"
    assert "pangalan" in sends.texts()[-1].lower()

    await post_message(api, make_message_event(psid=psid, text="Maria Santos"))
    await flush()
    assert await _active_step(psid) == "order_address"
    assert "address" in sends.texts()[-1].lower()

    await post_message(
        api, make_message_event(psid=psid, text="Blk 2 Lot 3 Masaito Residence, Kawit")
    )
    await flush()
    assert await _active_step(psid) == "order_contact"
    assert "number" in sends.texts()[-1].lower()

    await post_message(api, make_message_event(psid=psid, text="0917-123-4567"))
    await flush()

    # form done, customer now in team mode (bot paused), order saved
    assert not await _active_step(psid)
    assert await db.get_paused(psid)
    done = sends.texts()[-1]
    assert "Maria Santos" in done
    assert "2 iced latte" in done.lower()
    assert "confirm" in done.lower()

    rows = await db._fetchone_all("SELECT * FROM orders WHERE psid=?", (psid,))
    assert len(rows) == 1
    assert rows[0]["items"] == "2 iced latte + 1 banana bread"
    assert rows[0]["name"] == "Maria Santos"
    assert rows[0]["contact"] == "09171234567"  # normalized digits
    # menu didn't match anything: receipt is the raw text, no estimated total
    assert rows[0]["menu_note"] != ""
    assert rows[0]["total_estimate"] == ""

    assert len(tg["order"]) == 1
    assert tg["order"][0]["name"] == "Maria Santos"
    assert tg["order"][0]["total_estimate"] == ""


async def test_invalid_phone_gives_up_not_paused(api, sends, tg):
    psid = "PSID_ORD3"
    await post_message(api, make_message_event(psid=psid, text="gusto ko umorder"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="2 lattes"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="tama"))  # confirm step
    await flush()
    await post_message(api, make_message_event(psid=psid, text="Maria Santos"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="1 Masaito Rd Kawit"))
    await flush()
    for _ in range(3):
        await post_message(api, make_message_event(psid=psid, text="not a phone"))
        await flush()
    assert not await db.get_form(psid)
    assert not await db.get_paused(psid)


async def test_cancel_mid_form_releases_user(api, sends, tg):
    psid = "PSID_ORD4"
    await post_message(api, make_message_event(psid=psid, text="order"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="2 lattes"))
    await flush()
    assert await _active_step(psid) == "order_confirm"  # receipt shown first
    await post_message(api, make_message_event(psid=psid, text="wag na lang"))
    await flush()
    assert not await db.get_form(psid)
    assert not await db.get_paused(psid)
    assert "kinansel" in sends.texts()[-1]


async def test_delivery_question_answers_not_form(api, sends, tg):
    psid = "PSID_ORD5"
    await post_message(api, make_message_event(psid=psid, text="do you deliver po?"))
    await flush()
    # "deliver" alone is a delivery QUESTION, not an order intent
    assert not await db.get_form(psid)
    assert "FREE" in sends.texts()[-1]
    assert "Masaito" in sends.texts()[-1]


async def test_bulk_order_goes_to_team_not_form(api, sends, tg):
    psid = "PSID_ORD6"
    await post_message(
        api, make_message_event(psid=psid, text="hello po, I need a bulk order for our office para bukas")
    )
    await flush()
    assert await db.get_paused(psid)
    assert not await db.get_form(psid)
    assert len(tg["handoff"]) == 1


async def test_team_echo_resumes_paused_customer(api, sends, tg):
    psid = "PSID_ORD7"
    await db.set_paused(psid, True)
    echo = {
        "sender": {"id": "ERLBREW_PAGE"},
        "recipient": {"id": psid},
        "timestamp": 1,
        "message": {"is_echo": True, "mid": "echo.mid.1", "text": "Confirmed po your order!"},
    }
    from conftest import post_message as pm  # local alias for clarity
    await pm(api, echo)
    await flush()
    assert not await db.get_paused(psid)


async def test_admin_orders_endpoint(api, sends, tg):
    import config  # local import mirrors other admin endpoint tests

    headers = {"X-Admin-Token": config.settings.admin_token}
    resp = await api.get("/admin/orders", headers=headers)
    assert resp.status_code == 200
    data = resp.json()
    assert data["ok"] is True
    assert isinstance(data["orders"], list)

    resp = await api.get("/admin/orders")  # missing token
    assert resp.status_code == 403
    resp = await api.get("/admin/orders", headers={"X-Admin-Token": "wrong"})
    assert resp.status_code == 403


async def test_receipt_with_live_menu_prices_and_variant(api, sends, tg, menu_rows):
    psid = "PSID_ORD9"
    await post_message(api, make_message_event(psid=psid, text="order"))
    await flush()
    await post_message(
        api, make_message_event(psid=psid, text="2 white choco mocha + cookies")
    )
    await flush()
    assert await _active_step(psid) == "order_confirm"
    confirm = sends.texts()[-1]
    # real POS receipt: priced lines with the hot 8oz variant as the plain reading
    assert "2× WHITE CHOCOLATE MOCHA (8OZ) — ₱258" in confirm
    assert "1× CHOCOLATE COOKIES — ₱85" in confirm
    assert "Estimated total: ₱343" in confirm
    # sibling variant offered
    assert "iced 16oz ₱169" in confirm

    # confirm via quick reply payload routed through the same handler
    await post_message(api, make_message_event(psid=psid, quick_payload="ORD_OK"))
    await flush()
    assert await _active_step(psid) == "order_name"


async def test_receipt_edit_restarts_items(api, sends, tg, menu_rows):
    psid = "PSID_ORD10"
    await post_message(api, make_message_event(psid=psid, text="order"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="2 cookies"))
    await flush()
    assert await _active_step(psid) == "order_confirm"
    await post_message(api, make_message_event(psid=psid, text="mali po"))
    await flush()
    assert await _active_step(psid) == "order_items"
    row = await db.get_form(psid)
    assert "menu_note" not in row[1]  # stale receipt data cleared

    # finishing items again shows a fresh confirm (same machinery)
    await post_message(api, make_message_event(psid=psid, text="3 cookies"))
    await flush()
    assert await _active_step(psid) == "order_confirm"
    assert "3× CHOCOLATE COOKIES — ₱255" in sends.texts()[-1]


async def test_receipt_size_preference(api, sends, tg, menu_rows):
    psid = "PSID_ORD11"
    await post_message(api, make_message_event(psid=psid, text="order"))
    await flush()
    await post_message(
        api, make_message_event(psid=psid, text="1 iced white choco mocha 16oz")
    )
    await flush()
    confirm = sends.texts()[-1]
    assert "1× ICED WHITE CHOCOLATE MOCHA (16OZ) — ₱169" in confirm
    assert "Estimated total: ₱169" in confirm


async def test_menu_db_down_degrades_gracefully(api, sends, tg, monkeypatch):
    """MySQL unreachable: receipt shows the free-text items with no prices,
    and the order flow still completes."""
    import menu

    menu.reset_cache()

    async def broken():
        raise OSError("db down")

    monkeypatch.setattr(menu, "_fetch_rows", broken)
    psid = "PSID_ORD12"
    await post_message(api, make_message_event(psid=psid, text="order"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="jasmine green latte"))
    await flush()
    assert await _active_step(psid) == "order_confirm"
    confirm = sends.texts()[-1]
    assert "jasmine green latte" in confirm.lower()
    assert "₱" not in confirm
    await post_message(api, make_message_event(psid=psid, quick_payload="ORD_OK"))
    await flush()
    assert await _active_step(psid) == "order_name"  # flow continues


async def test_full_priced_flow_saves_total(api, sends, tg, menu_rows):
    psid = "PSID_ORD13"
    await post_message(api, make_message_event(psid=psid, text="paorder po"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="2 white choco mocha + cookies"))
    await flush()
    await post_message(api, make_message_event(psid=psid, quick_payload="ORD_OK"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="Maria Santos"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="1 Masaito Rd Kawit"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="09171234567"))
    await flush()
    assert await db.get_paused(psid)
    rows = await db._fetchone_all("SELECT * FROM orders WHERE psid=?", (psid,))
    assert rows[0]["total_estimate"] == "343"
    assert "WHITE CHOCOLATE MOCHA" in rows[0]["menu_note"]
    note = tg["order"][0]
    assert note["total_estimate"] == "343"
    assert "WHITE CHOCOLATE MOCHA" in note["menu_note"]


async def test_burst_webhooks_process_in_order(api, sends, tg):
    """A customer's rapid-fire messages must stay strictly serial (form-safe)."""
    psid = "PSID_ORD8"
    texts = ["pabili po", "2 iced latte guys", "cancel po"]
    posts = []
    for text in texts:
        ev = make_message_event(psid=psid, text=text)
        body = json.dumps(_webhook_body(ev), separators=(",", ":")).encode()
        headers = {"Content-Type": "application/json", **sign_headers(body, config.settings.app_secret)}
        posts.append(api.post("/webhook", content=body, headers=headers))
    responses = await asyncio.gather(*posts)
    assert all(r.status_code == 200 for r in responses)
    await flush()

    # Strict order: last burst message ("cancel po") released the form.
    assert not await db.get_form(psid)
    assert not await db.get_paused(psid)
    assert "kinansel" in sends.texts()[-1]
