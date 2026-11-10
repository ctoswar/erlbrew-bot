"""Deduplication by message id (mid) in SQLite."""
from __future__ import annotations

import db

from conftest import flush, make_message_event, post_message


async def test_same_mid_processed_once(api, sends, tg):
    event = make_message_event(mid="mid_dedupe_1", text="Hello there")

    await post_message(api, event)
    await post_message(api, event)
    await flush()

    texts = sends.texts()
    # First delivery: welcome flow -> exactly one user-facing text (menu match).
    assert len([t for t in texts if t]) == 1
    # Second delivery: silently dropped (but typing_on payloads may appear).
    assert ("text" not in texts[0]) or ("menu" in texts[0].lower() or "erl" in texts[0].lower())


async def test_duplicate_uses_same_reply(api, sends, tg):
    event = make_message_event(mid="mid_dedupe_2", text="magkano")
    await post_message(api, event)
    await flush()
    first = sends.message_payloads()
    count_after_first = len([p for p in first if p["message"].get("text")])

    await post_message(api, event)  # duplicate delivery, same mid
    await flush()
    second_texts = len([p for p in sends.message_payloads() if p["message"].get("text")])

    assert count_after_first == 1
    assert second_texts == 1


async def test_dedupe_persists_in_db(api, sends, tg):
    event = make_message_event(mid="mid_dedupe_3", text="hi")
    await post_message(api, event)
    await flush()
    # mid was stored by the first process; re-check via db directly
    assert await db.mark_message_seen("mid_dedupe_3") is False
    assert await db.mark_message_seen("mid_dedupe_4") is True
