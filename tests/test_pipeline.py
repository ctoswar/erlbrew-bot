"""Message pipeline ORDER tests (per spec):

  1 dedupe/rate-cap -> 2 paused -> 3 payloads -> 4 event form ->
  5 handoff keywords -> 6 FAQ -> 7 LLM fallback
"""
from __future__ import annotations

import config
import db

from conftest import flush, make_message_event, make_postback_event, post_message


async def test_step3_payload_beats_faq_and_llm(api, sends, tg, llm_marker):
    # Tapping Menu quick reply answers the menu entry; LLM must never be called.
    await post_message(api, make_message_event(psid="P_P1", quick_payload="MENU"))
    await flush()
    assert llm_marker.calls == []
    assert "erlbrew.codigital.services" in sends.texts()[-1]

    # Typed text matching a payload keyword also goes to FAQ (not LLM).
    await post_message(api, make_message_event(psid="P_P1", text="magkano po"))
    await flush()
    assert llm_marker.calls == []
    # menu content was filled in by the owner with the live menu link
    assert "erlbrew.codigital.services" in sends.texts()[-1]


async def test_step6_faq_beats_llm(api, sends, tg, llm_marker):
    await post_message(api, make_message_event(psid="P_P2", text="what time do you open?"))
    await flush()
    assert llm_marker.calls == []
    # owner filled in the real opening hours
    assert "8:00 AM" in sends.texts()[-1]


async def test_step7_freestext_goes_to_llm(api, sends, tg, llm_marker):
    await post_message(api, make_message_event(psid="P_P3", text="anong favorite color mo?"))
    await flush()
    assert llm_marker.calls == ["anong favorite color mo?"]
    assert sends.texts()[-1] == "LLM_SAYS_HI"


async def test_step7_fallback_message_on_llm_failure(api, sends, tg, llm_marker):
    # llm.generate (mocked here) returns None -> static fallback text is sent.
    # The Telegram failure notice itself is emitted inside the real llm module
    # and is covered by tests/test_llm.py.
    llm_marker.answer = None
    await post_message(api, make_message_event(psid="P_P4", text="beaconstring zzz"))
    await flush()
    assert sends.texts()[-1] == config.LLM_FALLBACK_TEXT


async def test_step4_form_continues_before_handoff_keywords(api, sends, tg):
    """Spec order: active event form (4) beats handoff keywords (5)."""
    psid = "P_P5"
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    # At event_type step: a complaint-like phrase is form input, not handoff.
    await post_message(api, make_message_event(psid=psid, text="unky i have a complaint"))
    await flush()
    step, _data, tries = await db.get_form(psid)
    assert step == "event_type"
    assert await db.get_paused(psid) is False
    assert sends.texts()[-1] != config.HANDOFF_TEXT
    assert tg["handoff"] == []

    # But after the form ENDS, the same phrase triggers the handoff (step 5).
    await post_message(api, make_message_event(psid=psid, text="cancel"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="i want to file a complaint"))
    await flush()
    assert await db.get_paused(psid) is True
    assert len(tg["handoff"]) == 1


async def test_paused_checked_before_payloads(api, sends, tg):
    """Step 2: paused means no reply at all — even to postback payloads."""
    psid = "P_P6"
    await db.upsert_user(psid)
    await db.set_paused(psid, True)  # pre-pause (setup equivalent)
    await post_message(api, make_postback_event(psid=psid, payload="MENU"))
    await flush()
    assert sends.texts() == []
    assert sends.quick_payloads_used() == []


async def test_non_text_message_gets_single_polite_reply(api, sends, tg, llm_marker):
    psid = "P_P7"
    await post_message(api, make_message_event(psid=psid, text=None, attachments=True))
    await flush()
    assert sends.texts() == [config.NON_TEXT_TEXT]
    assert llm_marker.calls == []

    await post_message(api, make_message_event(psid=psid, text=None, attachments=True, mid="othermid"))
    await flush()
    # Once per message (dedupe by mid ensures "reply once" per sticker)
    assert sends.texts().count(config.NON_TEXT_TEXT) == 2


async def test_echoes_are_ignored(api, sends, tg):
    await post_message(api, make_message_event(psid="P_P8", text="our own outgoing msg", is_echo=True))
    await flush()
    assert sends.texts() == []


async def test_form_payloadsent_inside_form_use_form(api, sends, tg, llm_marker):
    psid = "P_P9"
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    # EVT_TYPE payload goes to the FORM (step 4 chain), not to FAQ/LLM.
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_TYPE:Meeting"))
    await flush()
    step, data, _tries = await db.get_form(psid)
    assert step == "event_when"
    assert data["type"] == "Meeting"
    assert llm_marker.calls == []
