"""LLM fallback via a free local Ollama instance.

One public async function: generate(psid, user_text) -> Optional[str].
- A global asyncio.Semaphore(1) serializes generation (weak CPU-only box).
- A hard timeout (25 s default) covers queue wait + the HTTP call.
- A typing keeper re-sends typing_on every ~10 s while waiting.
- On timeout / any error: returns None; main then sends the static fallback
  and notifies Telegram. This function never raises.
- Per-user history: last N (6) messages, expired after 24 h of inactivity.
"""
from __future__ import annotations

import asyncio
import logging
import re
from pathlib import Path
from typing import Dict, List, Optional

import httpx

import config
import db
import messenger
import notify
from faq import HotFile

log = logging.getLogger("llm")

_semaphore: asyncio.Semaphore = asyncio.Semaphore(1)

# reply text; strip them defensively before it reaches the customer.
THINK_TAG_RE = re.compile(r"<think\b.*?</think\s*>", re.IGNORECASE | re.DOTALL)

_MAX_PROMPT_CHARS = 6000


def _load_prompt(path: Path) -> str:
    text = path.read_text(encoding="utf-8").strip()
    if not text:
        raise ValueError("knowledge.md is empty")
    return text


_knowledge: Optional[HotFile[str]] = None


def _knowledge_hot() -> HotFile[str]:
    global _knowledge
    if _knowledge is None:
        _knowledge = HotFile(config.settings.knowledge_path, _load_prompt, "knowledge.md")
    return _knowledge


def system_prompt() -> str:
    """Reload knowledge.md on mtime change; cap its size defensively."""
    try:
        text = _knowledge_hot().get()
    except (FileNotFoundError, ValueError, OSError) as exc:
        log.error("knowledge.md unavailable, minimal prompt used", extra={"error": str(exc)[:120]})
        text = (
            "You are Erl, the assistant of Erlbrew Cafe in Kawit, Cavite. "
            "Answer briefly and friendlily. Never guess prices, hours or "
            "availability. Decline unrelated questions politely."
        )
    return text[:_MAX_PROMPT_CHARS]


def build_messages(user_text: str, history: List[Dict[str, str]]) -> List[Dict[str, str]]:
    """System prompt + last N history turns + the new user message."""
    return (
        [{"role": "system", "content": system_prompt()}]
        + [
            {"role": m.get("role", "user"), "content": m.get("content", "")}
            for m in history
            if m.get("role") in ("user", "assistant")
        ]
        + [{"role": "user", "content": user_text}]
    )


def _chat_payload(messages: List[Dict[str, str]]) -> Dict[str, object]:
    s = config.settings
    return {
        "model": s.ollama_model,
        "messages": messages,
        "stream": False,
        "think": bool(s.ollama_think),
        "keep_alive": s.ollama_keep_alive,
        "options": {
            "num_predict": s.ollama_num_predict,
            "num_ctx": s.ollama_num_ctx,
            "temperature": s.ollama_temperature,
        },
    }


def _clean_answer(content: str) -> str:
    return THINK_TAG_RE.sub("", content or "").strip()


async def _post_chat(client: httpx.AsyncClient, messages: List[Dict[str, str]]) -> str:
    resp = await client.post(
        f"{config.settings.ollama_url}/api/chat", json=_chat_payload(messages)
    )
    resp.raise_for_status()
    data = resp.json()
    content = str(((data.get("message") or {}).get("content")) or "")
    return _clean_answer(content)


async def _run_locked(client: httpx.AsyncClient, messages: List[Dict[str, str]]) -> str:
    """Serialize generation site-wide; queued callers wait inside the timeout."""
    async with _semaphore:
        return await _post_chat(client, messages)


async def generate(
    psid: str,
    user_text: str,
    client: Optional[httpx.AsyncClient] = None,
    history: Optional[List[Dict[str, str]]] = None,
) -> Optional[str]:
    """LLM fallback. Returns the answer text, or None on timeout/failure.

    The optional client/history params exist for tests (no real HTTP calls).
    """
    if history is None:
        s = config.settings
        history = await db.get_history(psid, s.llm_max_history, s.llm_history_ttl_sec)
    messages = build_messages(user_text, history)

    own_client = client is None
    if own_client:
        client = httpx.AsyncClient(timeout=config.settings.llm_timeout_sec)
    assert client is not None

    messenger.start_typing(psid)
    try:
        try:
            answer = await asyncio.wait_for(
                _run_locked(client, messages),
                timeout=config.settings.llm_timeout_sec,
            )
        except asyncio.TimeoutError:
            log.warning(
                "llm timeout",
                extra={"psid": psid, "timeout_sec": config.settings.llm_timeout_sec},
            )
            await notify.notify_llm_failure(psid, "timeout")
            return None
        except Exception as exc:  # httpx errors, bad JSON, Ollama 4xx/5xx, ...
            log.warning(
                "llm failure",
                extra={"psid": psid, "error": type(exc).__name__},
            )
            await notify.notify_llm_failure(psid, type(exc).__name__)
            return None
    finally:
        messenger.stop_typing(psid)
        if own_client:
            await client.aclose()

    answer = _clean_answer(answer)
    if not answer:
        log.warning("llm empty answer", extra={"psid": psid})
        await notify.notify_llm_failure(psid, "empty answer")
        return None

    await db.append_history(psid, "user", user_text)
    await db.append_history(psid, "assistant", answer)
    return answer
