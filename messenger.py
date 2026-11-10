"""Facebook Messenger send side (Graph API) via httpx — async, never raises to callers.

- Uses `Authorization: Bearer <page token>` so tokens never appear in URLs/logs.
- All public functions swallow API errors and return False after logging
  (status code + short body only, no secrets).
- `post_graph` is the single chokepoint, which the tests patch to mock HTTP.
"""
from __future__ import annotations

import asyncio
import logging
from typing import Any, Dict, List, Optional, Tuple

import httpx

import config

log = logging.getLogger("messenger")

GRAPH_BASE = "https://graph.facebook.com"
_REQUEST_TIMEOUT = 12.0


class MessengerError(RuntimeError):
    def __init__(self, op: str, status: int, detail: str):
        super().__init__(f"graph error op={op} status={status} detail={detail[:300]}")
        self.op = op
        self.status = status


_client: Optional[httpx.AsyncClient] = None


def _http() -> httpx.AsyncClient:
    global _client
    if _client is None or _client.is_closed:
        _client = httpx.AsyncClient(timeout=_REQUEST_TIMEOUT)
    return _client


async def aclose() -> None:
    global _client
    if _client is not None and not _client.is_closed:
        await _client.aclose()
    _client = None


async def post_graph(op: str, payload: Dict[str, Any]) -> Dict[str, Any]:
    """POST to {GRAPH_BASE}/{version}/{op}. Returns parsed JSON or raises MessengerError."""
    url = f"{GRAPH_BASE}/{config.settings.graph_version}/{op}"
    try:
        resp = await _http().post(
            url,
            json=payload,
            headers={"Authorization": f"Bearer {config.settings.page_access_token}"},
        )
    except (httpx.HTTPError, OSError) as exc:
        raise MessengerError(op, 0, f"network: {type(exc).__name__}") from exc
    try:
        data: Dict[str, Any] = resp.json()
    except ValueError:
        data = {}
    if resp.status_code >= 400 or "error" in data:
        err = data.get("error") or {}
        raise MessengerError(op, resp.status_code, str(err.get("message") or resp.text[:200]))
    return data


async def get_graph(path: str, fields: str) -> Dict[str, Any]:
    """GET {GRAPH_BASE}/{version}/{path}?fields=... (e.g. user profile)."""
    url = f"{GRAPH_BASE}/{config.settings.graph_version}/{path}"
    try:
        resp = await _http().get(
            url,
            params={"fields": fields, "access_token": config.settings.page_access_token},
        )
    except (httpx.HTTPError, OSError) as exc:
        raise MessengerError(path, 0, f"network: {type(exc).__name__}") from exc
    try:
        data: Dict[str, Any] = resp.json()
    except ValueError:
        data = {}
    if resp.status_code >= 400 or "error" in data:
        err = data.get("error") or {}
        raise MessengerError(path, resp.status_code, str(err.get("message") or resp.text[:200]))
    return data


# --- sends --------------------------------------------------------------------
def _message_payload(
    psid: str, text: str, quick_replies: Optional[List[Tuple[str, str]]] = None
) -> Dict[str, Any]:
    message: Dict[str, Any] = {"text": text[:2000]}
    if quick_replies:
        # Quick-reply titles max 20 characters on Messenger.
        message["quick_replies"] = [
            {"content_type": "text", "title": t[:20], "payload": p}
            for t, p in quick_replies
        ]
    return {"recipient": {"id": psid}, "message": message}


async def send_text(psid: str, text: str) -> bool:
    return await send_quick_replies(psid, text, None)


async def send_quick_replies(
    psid: str, text: str, quick_replies: Optional[List[Tuple[str, str]]]
) -> bool:
    try:
        await post_graph("me/messages", _message_payload(psid, text, quick_replies))
        return True
    except MessengerError as exc:
        log.error("send failed", extra={"op": exc.op, "status": exc.status, "detail": str(exc)[:160]})
        return False


async def send_typing(psid: str, action: str = "typing_on") -> bool:
    try:
        await post_graph(
            "me/messages", {"recipient": {"id": psid}, "sender_action": action}
        )
        return True
    except MessengerError as exc:
        log.error("typing failed", extra={"op": exc.op, "status": exc.status})
        return False


async def get_profile_name(psid: str) -> Optional[str]:
    """Best-effort profile fetch; returns None on any error (dev mode limits this)."""
    try:
        data = await get_graph(psid, fields="first_name,last_name")
    except MessengerError as exc:
        log.warning("profile fetch failed", extra={"status": exc.status, "detail": str(exc)[:120]})
        return None
    parts = [str(data.get("first_name", "")), str(data.get("last_name", ""))]
    name = " ".join(p.strip() for p in parts if p.strip())
    return name or None


# --- typing keeper ---------------------------------------------------------------
_typing_tasks: Dict[str, asyncio.Task[None]] = {}


async def _typing_loop(psid: str, interval: float) -> None:
    """Re-send typing_on every `interval` seconds until cancelled."""
    while True:
        await asyncio.sleep(interval)
        await send_typing(psid, "typing_on")


def start_typing(psid: str, interval: Optional[float] = None) -> None:
    """Start (or restart) the typing keeper for a PSID. Fire-and-forget."""
    stop_typing(psid)
    if interval is None:
        interval = config.settings.typing_resend_sec
    _typing_tasks[psid] = asyncio.create_task(_typing_loop(psid, interval))


def stop_typing(psid: str) -> None:
    task = _typing_tasks.pop(psid, None)
    if task is not None and not task.done():
        task.cancel()
