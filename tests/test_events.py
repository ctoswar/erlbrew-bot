"""Event form state machine: steps, validation, cancel, completion."""
from __future__ import annotations

import db
import events

from conftest import flush, make_message_event, post_message


async def test_full_happy_flow_saves_lead(api, sends, tg):
    psid = "PSID_EVT1"
    # Start via the EVENTS payload -> static answer with a booking quick reply.
    await post_message(api, make_message_event(psid=psid, quick_payload="EVENTS"))
    await flush()
    assert "[PLACEHOLDER:" in sends.texts()[-1]  # static events answer
    assert ("Start booking", "EVT_START") in sends.quick_payloads_used()[-1]

    # Start the form.
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    assert await events.has_active_form(psid)
    assert sends.quick_payloads_used()[-1] == events._type_tiles()
    assert "event" in sends.texts()[-1].lower()

    # Step: event type (payload quick reply)
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_TYPE:Birthday"))
    await flush()
    assert "petsa" in sends.texts()[-1].lower()

    # Step: date/time
    await post_message(api, make_message_event(psid=psid, text="May 12, 9 AM po"))
    await flush()
    assert "guests" in sends.texts()[-1].lower()

    # Step: guests
    await post_message(api, make_message_event(psid=psid, text="12 guests po"))
    await flush()
    assert "pangalan" in sends.texts()[-1].lower()

    # Step: name
    await post_message(api, make_message_event(psid=psid, text="Maria Santos"))
    await flush()
    assert "number" in sends.texts()[-1].lower()

    # Step: contact -> completion
    await post_message(api, make_message_event(psid=psid, text="0917-123-4567"))
    await flush()

    assert not await events.has_active_form(psid)
    done = sends.texts()[-1]
    assert "Maria Santos" in done
    assert "24" in done  # EVENT_REPLY_HOURS mentioned
    assert "confirm" in done.lower() or "hindi" in done.lower()

    rows = await db._fetchone_all("SELECT * FROM event_leads WHERE psid=?", (psid,))
    assert len(rows) == 1
    lead = rows[0]
    assert lead["event_type"] == "Birthday"
    assert lead["event_when"] == "May 12, 9 AM po"
    assert lead["guests"] == 12
    assert lead["name"] == "Maria Santos"
    assert lead["contact"] == "09171234567"

    assert len(tg["event_lead"]) == 1
    assert tg["event_lead"][0]["event_type"] == "Birthday"
    assert tg["event_lead"][0]["contact"] == "09171234567"


async def test_typed_event_type_also_accepted(api, sends, tg):
    psid = "PSID_EVT2"
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="photoshoot po"))
    await flush()
    step, data, _ = await db.get_form(psid)
    assert step == "event_when"
    assert data["type"] == "Photoshoot"


async def test_invalid_guests_reask_then_invalid_contact(api, sends, tg):
    psid = "PSID_EVT3"
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_TYPE:Meeting"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="next Monday 2pm"))
    await flush()

    # Invalid guests -> re-ask (hint).
    await post_message(api, make_message_event(psid=psid, text="marami po"))
    await flush()
    step, data, tries = await db.get_form(psid)
    assert step == "guests"
    assert tries == 1
    assert "number" in sends.texts()[-1].lower()

    # Valid guests -> name question.
    await post_message(api, make_message_event(psid=psid, text="8"))
    await flush()
    step, data, tries = await db.get_form(psid)
    assert step == "name"
    assert tries == 0
    assert data["guests"] == "8"

    # Name -> contact question.
    await post_message(api, make_message_event(psid=psid, text="Juan"))
    await flush()
    step, data, tries = await db.get_form(psid)
    assert step == "contact"

    # Invalid contact -> re-ask with hint.
    await post_message(api, make_message_event(psid=psid, text="hello phone"))
    await flush()
    step, data, tries = await db.get_form(psid)
    assert step == "contact"
    assert tries == 1
    assert "mobile" in sends.texts()[-1].lower() or "number" in sends.texts()[-1].lower()

    # Valid contact -> completion.
    await post_message(api, make_message_event(psid=psid, text="+639171234567"))
    await flush()
    rows = await db._fetchone_all("SELECT psid, name FROM event_leads WHERE psid=?", (psid,))
    assert len(rows) == 1
    assert rows[0]["name"] == "Juan"


async def test_three_invalid_inputs_cancel_form(api, sends, tg):
    psid = "PSID_EVT4"
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="walang spill"))  # invalid type 1
    await flush()
    await post_message(api, make_message_event(psid=psid, text="siguro whatever"))  # invalid type 2
    await flush()
    assert await events.has_active_form(psid)
    await post_message(api, make_message_event(psid=psid, text="hmm idk po"))  # invalid type 3
    await flush()
    assert not await events.has_active_form(psid)
    assert "kinansel" in sends.texts()[-1]


async def test_cancel_any_step(api, sends, tg):
    psid = "PSID_EVT5"
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_START"))
    await flush()
    await post_message(api, make_message_event(psid=psid, quick_payload="EVT_TYPE:Other"))
    await flush()
    await post_message(api, make_message_event(psid=psid, text="cancel po"))
    await flush()
    assert not await events.has_active_form(psid)
    assert "kinansel" in sends.texts()[-1]
    rows = await db._fetchone_all("SELECT * FROM event_leads WHERE psid=?", (psid,))
    assert rows == []


async def test_lenient_ph_contact_formats(api, sends, tg):
    for contact in ("09171234567", "+639171234567", "639 171 234 567", "0917-123-4567"):
        psid = f"PSID_EVT6_{contact}"
        await db.delete_form(psid)
        await events.start_form(psid)
        await events.handle_input(psid, "EVT_TYPE:Birthday")
        await events.handle_input(psid, "May 12 9am")
        await events.handle_input(psid, "5")
        await events.handle_input(psid, "Test User")
        await events.handle_input(psid, contact)
        rows = await db._fetchone_all("SELECT contact FROM event_leads WHERE psid=?", (psid,))
        assert len(rows) == 1, contact
