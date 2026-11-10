"""Per-user rate limiting: 15 messages / 10 minutes, one polite notice."""
from __future__ import annotations

import config

from conftest import flush, make_message_event, post_message


async def _send(api, psid: str, n: int) -> None:
    for i in range(n):
        event = make_message_event(psid=psid, text=f"msg {i} asdf{i}")
        await post_message(api, event)
        await flush()


async def test_rate_limit_triggers_once_then_silence(api, sends, tg):
    psid = "PSID_RATE_1"

    # 15 messages are fine: each reaches the LLM-fallback reply path.
    await _send(api, psid, config.settings.rate_limit_max)
    assert len(sends.texts()) == config.settings.rate_limit_max

    # 16th message exceeds the limit: one polite notice (then ignore).
    await _send(api, psid, 1)
    assert sends.texts()[-1] == config.RATE_LIMITED_TEXT

    # 17th (and later) messages: ignored, no further notices.
    before = len(sends.texts())
    await _send(api, psid, 3)
    assert len(sends.texts()) == before
    assert sends.texts().count(config.RATE_LIMITED_TEXT) == 1


async def test_rate_limit_is_per_user(api, sends, tg):
    # Fill the limit for user A...
    for i in range(config.settings.rate_limit_max + 2):
        await post_message(api, make_message_event(psid="PSID_RATE_A", text=f"hi {i} pwd"))
        await flush()
    assert config.RATE_LIMITED_TEXT in sends.texts()

    # ...user B is a different bucket entirely.
    await post_message(api, make_message_event(psid="PSID_RATE_B", text="hello po"))
    await flush()
    assert sends.texts()[-1] != config.RATE_LIMITED_TEXT


async def test_too_long_message_notice_then_ignore(api, sends, tg):
    psid = "PSID_LONG_1"
    long_text = "x" * (config.settings.max_text_chars + 1)
    await post_message(api, make_message_event(psid=psid, text=long_text))
    await flush()
    assert sends.texts() == [config.TOO_LONG_TEXT]

    await post_message(api, make_message_event(psid=psid, text=long_text + "y"))
    await flush()
    # second long message: ignored silently (the polite notice was already sent)
    assert sends.texts() == [config.TOO_LONG_TEXT]

