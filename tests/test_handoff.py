"""Handoff keywords -> short message, pause user, Telegram notification."""
from __future__ import annotations

import config
import db

from conftest import flush, make_message_event, make_postback_event, post_message


async def test_refund_keyword_pauses_user(api, sends, tg):
    await post_message(api, make_message_event(psid="PSID_H1", text="I want a refund po"))
    await flush()

    texts = sends.texts()
    assert texts == [config.HANDOFF_TEXT]
    assert len(tg["handoff"]) == 1
    assert tg["handoff"][0]["psid"] == "PSID_H1"
    assert "refund" in tg["handoff"][0]["message"].lower()
    assert await db.get_paused("PSID_H1") is True


async def test_tao_and_reklamo_keywords(api, sends, tg):
    await post_message(api, make_message_event(psid="PSID_H2", text="kailangan ko ng tao"))
    await flush()
    assert await db.get_paused("PSID_H2") is True
    assert tg["handoff"][-1]["psid"] == "PSID_H2"

    await post_message(api, make_message_event(psid="PSID_H3", text="REKLAMO AKO!!!"))
    await flush()
    assert await db.get_paused("PSID_H3") is True


async def test_allergy_and_bulk_order_keywords(api, sends, tg):
    await post_message(api, make_message_event(psid="PSID_H4", text="my sister is allergic po"))
    await flush()
    assert await db.get_paused("PSID_H4") is True

    await post_message(api, make_message_event(psid="PSID_H5", text="Bulk order for a wedding"))
    await flush()
    assert await db.get_paused("PSID_H5") is True


async def test_paused_user_silenced(api, sends, tg):
    psid = "PSID_H6"
    await post_message(api, make_message_event(psid=psid, text="reklamo po ito"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="hello again"))
    await flush()

    # Only the handoff message; the follow-up got nothing.
    assert sends.texts() == [config.HANDOFF_TEXT]

    # Even postbacks are ignored while paused.
    await post_message(api, make_postback_event(psid=psid, payload="MENU"))
    await flush()
    assert len(sends.quick_payloads_used()) == 0


async def test_admin_unpause_resumes_replies(api, sends, tg):
    psid = "PSID_H7"
    await post_message(api, make_message_event(psid=psid, text="need human help po"))
    await flush()
    assert await db.get_paused(psid) is True
    before = len(sends.texts())

    resp = await api.post(
        f"/admin/unpause/{psid}", headers={"X-Admin-Token": config.settings.admin_token}
    )
    assert resp.status_code == 200
    assert await db.get_paused(psid) is False

    await post_message(api, make_message_event(psid=psid, text="magkano ang latte"))
    await flush()
    # The user got an answer again (no new handoff / silence).
    assert len(sends.texts()) == before + 1
    assert sends.texts()[-1] != config.HANDOFF_TEXT


async def test_non_handoff_question_not_paused(api, sends, tg):
    psid = "PSID_H8"
    await post_message(api, make_message_event(psid=psid, text="gusto ko ng iced latte"))
    await flush()
    assert await db.get_paused(psid) is False
    assert tg["handoff"] == []
