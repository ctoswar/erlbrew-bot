"""LLM fallback tests: Ollama payload, history, timeout, failure handling.

All HTTP is mocked with httpx.MockTransport — no real Ollama calls; Telegram
and Messenger sends are mocked by the shared fixtures.
"""
from __future__ import annotations

import asyncio
import dataclasses
import json
import time
from typing import Any, Dict, List

import httpx

import config
import db
import llm

_OK_CONTENT = (
    " Sure po! Answer here. " + "<" + "think>" + "hidden reasoning" + "<" + "/think>"
)
_OK_BODY: Dict[str, Any] = {"message": {"role": "assistant", "content": _OK_CONTENT}}
_CLEAN_EXPECTED = "Sure po! Answer here."


class Rec:
    def __init__(self) -> None:
        self.requests: List[httpx.Request] = []

    def transport(self, json_body: Dict[str, Any], delay: float = 0.0) -> httpx.MockTransport:
        rec = self

        def handler(request: httpx.Request) -> httpx.Response:
            rec.requests.append(request)
            if delay:
                time.sleep(delay)
            return httpx.Response(200, json=json_body)

        return httpx.MockTransport(handler)


async def test_generate_success_and_payload_shape(tg, sends):
    rec = Rec()
    async with httpx.AsyncClient(transport=rec.transport(_OK_BODY)) as client:
        answer = await llm.generate("PSID_L1", "sino ka po?", client=client)

    assert answer == _CLEAN_EXPECTED  # think tags stripped, trimmed
    req = rec.requests[-1]
    assert req.url.path.endswith("/api/chat")
    body = json.loads(req.content.decode())
    assert body["model"] == config.settings.ollama_model
    assert body["stream"] is False
    assert body["think"] is False
    assert body["keep_alive"] == "30m"
    assert body["options"] == {"num_predict": 120, "num_ctx": 2048, "temperature": 0.2}
    assert body["messages"][0]["role"] == "system"
    assert body["messages"][-1] == {"role": "user", "content": "sino ka po?"}

    # History roundtrip: user + assistant stored.
    hist = await db.get_history("PSID_L1", 6, 86400)
    assert hist[-2] == {"role": "user", "content": "sino ka po?"}
    assert hist[-1] == {"role": "assistant", "content": _CLEAN_EXPECTED}


async def test_history_capped_at_six(tg, sends):
    for i in range(9):
        await db.append_history("PSID_L2", "user", f"u{i}")
        await db.append_history("PSID_L2", "assistant", f"a{i}")
    msgs = llm.build_messages("current", await db.get_history("PSID_L2", 6, 86400))
    assert msgs[0]["role"] == "system"
    assert len(msgs) == 1 + 6 + 1
    contents = [m["content"] for m in msgs]
    assert contents[-1] == "current"
    # last 6 entries of 18: u6,a6,u7,a7,u8,a8
    assert "u8" in contents
    assert "u6" in contents
    assert "u5" not in contents
    assert "a5" not in contents


async def test_history_ttl_via_cutoff(tg, sends):
    await db.append_history("PSID_L3", "user", "old question")
    stale = time.time() - 25 * 3600
    await db._conn.execute(
        "UPDATE chat_history SET created_at = ? WHERE psid = 'PSID_L3'", (stale,)
    )
    await db._conn.commit()
    hist = await db.get_history("PSID_L3", 6, config.settings.llm_history_ttl_sec)
    assert hist == []


async def test_error_returns_none_and_notifies(tg, sends):
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(500, json={"error": "boom"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        answer = await llm.generate("PSID_L4", "question zzz", client=client)

    assert answer is None
    assert len(tg["llm_failure"]) == 1
    assert tg["llm_failure"][0]["psid"] == "PSID_L4"
    # Nothing was stored in history for a failed exchange.
    assert await db.get_history("PSID_L4", 6, 3600) == []


async def test_timeout_returns_none_and_notifies(monkeypatch, tg, sends):
    old = config.settings
    fast = dataclasses.replace(old, llm_timeout_sec=0.05)
    monkeypatch.setattr(config, "settings", fast)

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=_OK_BODY)

    async def slow_async_handler(request: httpx.Request) -> httpx.Response:
        # Async handler so the awaited sleep can be cancelled by wait_for.
        await asyncio.sleep(0.2)
        return httpx.Response(200, json=_OK_BODY)

    async with httpx.AsyncClient(transport=httpx.MockTransport(slow_async_handler)) as client:
        answer = await llm.generate("PSID_L5", "question zzz", client=client)

    assert answer is None
    assert len(tg["llm_failure"]) == 1
    assert tg["llm_failure"][0]["reason"] == "timeout"
    assert await db.get_history("PSID_L5", 6, 3600) == []


async def test_semaphore_serializes_generations(tg, sends):
    rec = Rec()

    async def handler(request: httpx.Request) -> httpx.Response:
        rec.requests.append(request)
        await asyncio.sleep(0.05)  # truly async: could overlap without the lock
        return httpx.Response(200, json=_OK_BODY)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        started_at = time.monotonic()
        results = await asyncio.gather(
            llm.generate("PSID_S1", "one", client=client),
            llm.generate("PSID_S1", "two", client=client),
            llm.generate("PSID_S1", "three", client=client),
        )
        elapsed = time.monotonic() - started_at

    assert all(r == _CLEAN_EXPECTED for r in results)
    assert len(rec.requests) == 3
    # One-at-a-time: three 50 ms generations cannot finish in under ~100 ms.
    assert elapsed >= 0.09
