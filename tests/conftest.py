"""Shared fixtures for the Erlbrew Café bot tests.

- Sets env vars BEFORE any app module is imported (config reads env at import).
- Fresh SQLite database per test (tmp_path).
- Mocked HTTP: messenger.post_graph / get_profile_name are replaced with a
  recorder (no real Meta calls); the three notify_* functions are replaced
  (no real Telegram calls); llm.generate is patched marker-style per test.
"""
from __future__ import annotations

import hashlib
import hmac
import json
import os
import secrets
import time
from typing import Any, Dict, List, Optional

import httpx
import pytest_asyncio

# --- environment (must run before `import config` anywhere) -------------------
_TEST_ENV: Dict[str, str] = {
    "APP_SECRET": "test_app_secret_abc123",
    "VERIFY_TOKEN": "test_verify_token",
    "PAGE_ACCESS_TOKEN": "test_page_token",
    "GRAPH_VERSION": "v23.0",
    "ADMIN_TOKEN": "test_admin_token",
    # Point Ollama at a dead port: any accidental real call fails fast, offline.
    "OLLAMA_BASE_URL": "http://127.0.0.1:9",
    "OLLAMA_MODEL": "test-model:2b",
    "TELEGRAM_BOT_TOKEN": "",
    "TELEGRAM_CHAT_ID": "",
    "EVENT_REPLY_HOURS": "24",
    "RATE_LIMIT_MAX": "15",
    "RATE_LIMIT_WINDOW_SEC": "600",
    "MAX_TEXT_CHARS": "500",
    "LLM_TIMEOUT_SEC": "25",
    "LOG_LEVEL": "WARNING",
}
for _k, _v in _TEST_ENV.items():
    os.environ[_k] = _v

import config  # noqa: E402
import db  # noqa: E402
import llm  # noqa: E402
import main  # noqa: E402
import messenger  # noqa: E402
import notify  # noqa: E402


class SendRecorder:
    """Collects every Graph 'send' payload the pipeline would fire."""

    def __init__(self) -> None:
        self.items: List[tuple[str, Dict[str, Any]]] = []

    @property
    def payloads(self) -> List[Dict[str, Any]]:
        return [p for (_op, p) in self.items]

    def texts(self) -> List[str]:
        out = []
        for p in self.items:
            msg = p[1].get("message")
            if isinstance(msg, dict) and "text" in msg:
                out.append(str(msg["text"]))
        return out

    def message_payloads(self) -> List[Dict[str, Any]]:
        return [p[1] for p in self.items if "message" in p[1]]

    def quick_payloads_used(self) -> List[Any]:
        out = []
        for p in self.items:
            msg = p[1].get("message")
            if isinstance(msg, dict) and "quick_replies" in msg:
                out.append([(q.get("title"), q.get("payload")) for q in msg["quick_replies"]])
        return out


@pytest_asyncio.fixture(autouse=True)
async def database(tmp_path) -> Any:
    await db.close()
    await db.connect(str(tmp_path / "test.db"))
    import menu
    menu.reset_cache()  # no cross-test cache bleed (fresh per test)
    yield
    await db.close()
    menu.reset_cache()


@pytest_asyncio.fixture
async def sends(monkeypatch) -> SendRecorder:
    """Mock Messenger sends + profile fetch (all Graph HTTP mocked)."""
    rec = SendRecorder()

    async def fake_post_graph(op: str, payload: Dict[str, Any]) -> Dict[str, Any]:
        rec.items.append((op, payload))
        result: Dict[str, Any] = {"recipient_id": "1000000000"}
        if "message" in payload:
            result["message_id"] = "mid_test"
        return result

    async def fake_profile(psid: str) -> Optional[str]:
        return "Test User"

    monkeypatch.setattr(messenger, "post_graph", fake_post_graph)
    monkeypatch.setattr(messenger, "get_profile_name", fake_profile)
    return rec


@pytest_asyncio.fixture
async def tg(monkeypatch) -> Dict[str, Any]:
    """Mock Telegram notifications, recorded by kind."""
    rec: Dict[str, List[Dict[str, Any]]] = {"event_lead": [], "handoff": [], "llm_failure": [], "order": []}

    async def fake_lead(**kwargs: Any) -> bool:
        rec["event_lead"].append(kwargs)
        return True

    async def fake_order(
        psid: str,
        *,
        name: str,
        items: str,
        address: str,
        contact: str,
        menu_note: str = "",
        total_estimate: str = "",
    ) -> bool:
        rec["order"].append(
            {
                "psid": psid,
                "name": name,
                "items": items,
                "address": address,
                "contact": contact,
                "menu_note": menu_note,
                "total_estimate": total_estimate,
            }
        )
        return True

    async def fake_handoff(psid: str, name: str, message: str) -> bool:
        rec["handoff"].append({"psid": psid, "name": name, "message": message})
        return True

    async def fake_llm_failure(psid: str, reason: str) -> bool:
        rec["llm_failure"].append({"psid": psid, "reason": reason})
        return True

    monkeypatch.setattr(notify, "notify_event_lead", fake_lead)
    monkeypatch.setattr(notify, "notify_order", fake_order)
    monkeypatch.setattr(notify, "notify_handoff", fake_handoff)
    monkeypatch.setattr(notify, "notify_llm_failure", fake_llm_failure)
    return rec


@pytest_asyncio.fixture
def llm_marker(monkeypatch):
    """Patch llm.generate with a marker; records whether it was called."""

    class Marker:
        def __init__(self) -> None:
            self.calls: List[str] = []
            self.answer: Optional[str] = "LLM_SAYS_HI"

        async def __call__(self, psid: str, text: str, *a: Any, **kw: Any) -> Optional[str]:
            self.calls.append(text)
            return self.answer

    marker = Marker()
    monkeypatch.setattr(llm, "generate", marker)
    return marker


# ---------------------------------------------------------------------------
# Webhook helpers
# ---------------------------------------------------------------------------
def make_message_event(
    psid: str = "PSID_TESTS",
    mid: Optional[str] = None,
    text: Optional[str] = None,
    quick_payload: Optional[str] = None,
    attachments: bool = False,
    is_echo: bool = False,
) -> Dict[str, Any]:
    if mid is None:
        mid = f"mid.{int(time.time() * 1000)}.{secrets.token_hex(6)}"
    msg: Dict[str, Any] = {"mid": mid, "seq": 1}
    if text is not None:
        msg["text"] = text
    if quick_payload:
        msg["quick_reply"] = {"payload": quick_payload}
        msg.setdefault("text", quick_payload)
    if attachments:
        msg["attachments"] = [{"type": "image", "payload": {"url": "x"}}]
        msg.setdefault("text", "")
    if is_echo:
        msg["is_echo"] = True
    return {
        "sender": {"id": psid},
        "recipient": {"id": "PAGE"},
        "timestamp": int(time.time() * 1000),
        "message": msg,
    }


def make_postback_event(psid: str = "PSID_TESTS", payload: str = "MENU") -> Dict[str, Any]:
    return {
        "sender": {"id": psid},
        "recipient": {"id": "PAGE"},
        "timestamp": int(time.time() * 1000),
        "postback": {"title": payload.title() if payload else "", "payload": payload},
    }


def _webhook_body(event: Dict[str, Any]) -> Dict[str, Any]:
    return {"object": "page", "entry": [{"id": "PAGE", "time": int(time.time() * 1000), "messaging": [event]}]}


def sign_headers(body: bytes, secret: str) -> Dict[str, str]:
    digest = hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    return {"Content-Type": "application/json", "X-Hub-Signature-256": f"sha256={digest}"}


async def flush() -> None:
    """Let background tasks created by the POST handler run to completion."""
    for _ in range(30):
        await asyncio.sleep(0)
    await asyncio.sleep(0.02)


import asyncio  # noqa: E402  (kept last to mark it as a local helper)

@pytest_asyncio.fixture
async def api(monkeypatch):
    from httpx import ASGITransport

    transport = ASGITransport(app=main.app)
    client = httpx.AsyncClient(transport=transport, base_url="http://test")
    yield client
    await client.aclose()


async def post_message(
    api,
    event: Dict[str, Any],
    secret: str = config.settings.app_secret,
    with_signature: bool = True,
) -> httpx.Response:
    body = json.dumps(_webhook_body(event), separators=(",", ":")).encode()
    headers: Dict[str, str] = {"Content-Type": "application/json"}
    if with_signature:
        headers.update(sign_headers(body, secret))
    return await api.post("/webhook", content=body, headers=headers)
