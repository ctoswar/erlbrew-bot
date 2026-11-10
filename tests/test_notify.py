"""Telegram notify module: formatting, no-leak behaviour, disabled state."""
from __future__ import annotations

import dataclasses
import json

import httpx

import config
import notify


async def test_disabled_when_not_configured(monkeypatch):
    """No Telegram token/chat -> functions return False and send nothing."""
    disabled = dataclasses.replace(
        config.settings, telegram_bot_token="", telegram_chat_id=""
    )
    monkeypatch.setattr(config, "settings", disabled)

    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, json={"ok": True})

    transport = httpx.MockTransport(handler)

    async with httpx.AsyncClient(transport=transport) as client:
        assert await notify.send_telegram("hello", client=client) is False
        assert await notify.notify_handoff("p", "n", "m") is False
        assert await notify.notify_event_lead(
            psid="p", event_type="t", event_when="w", guests=1, name="n", contact="c"
        ) is False
        assert await notify.notify_llm_failure("p", "why") is False

    assert seen == []  # nothing was sent at all


async def test_send_telegram_posts_expected_body(monkeypatch):
    monkeypatch.setattr(
        config,
        "settings",
        dataclasses.replace(config.settings, telegram_bot_token="testtg", telegram_chat_id="123"),
    )
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(200, json={"ok": True})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        ok = await notify.send_telegram("Test message", client=client)

    assert ok is True
    req = requests[-1]
    assert "/bot" in req.url.path
    assert config.settings.telegram_bot_token in req.url.path
    assert "?" not in str(req.url)  # token in URL query? must never happen
    body = json.loads(req.content.decode())
    assert body["chat_id"] == "123" and body["text"] == "Test message"


async def test_handoff_notification_has_info_no_secrets(monkeypatch):
    monkeypatch.setattr(
        config, "settings", dataclasses.replace(config.settings, telegram_bot_token="testtg", telegram_chat_id="123")
    )
    snips: list[str] = []

    async def capture(text: str, client: httpx.AsyncClient | None = None) -> bool:
        snips.append(text)
        return True

    monkeypatch.setattr(notify, "send_telegram", capture)
    await notify.notify_handoff("PSID_N1", "Test User", "I need a refund now pls")

    text = snips[0]
    assert "PSID_N1" in text and "refund" in text and "HANDOFF" in text
    assert config.settings.page_access_token not in text
    assert config.settings.app_secret not in text
    assert "testtg" not in text


async def test_event_lead_notification_lists_details(monkeypatch):
    monkeypatch.setattr(
        config, "settings", dataclasses.replace(config.settings, telegram_bot_token="testtg", telegram_chat_id="123")
    )
    snips: list[str] = []

    async def capture(text: str, client: httpx.AsyncClient | None = None) -> bool:
        snips.append(text)
        return True

    monkeypatch.setattr(notify, "send_telegram", capture)
    await notify.notify_event_lead(
        psid="PSID_N2", event_type="Meeting", event_when="May 12 3pm",
        guests=10, name="Juan", contact="09171234567",
    )

    text = snips[0]
    for needle in ("Juan", "Meeting", "May 12", "09171234567", "10"):
        assert needle in text
    assert "NOT confirmed" in text
