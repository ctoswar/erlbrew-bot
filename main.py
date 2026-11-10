"""Erlbrew Café — Facebook Messenger webhook service (rule-based first).

Message pipeline (in order):
  1. Skip echoes; dedupe by message id; per-user rate limit (15/10 min) and
     500-char text cap (one polite notice, then ignore).
  2. Paused users (human handoff) get no replies.
  3. Postbacks / quick replies (MENU, HOURS, PROMO, EVENTS, GET_STARTED, ...)
     answer from faq.yaml.
  4. An active event form continues (deterministic state machine).
  5. Handoff keywords -> short message, pause, Telegram notification.
  6. Keyword/regex FAQ match -> static Taglish answer.
  7. Ollama LLM fallback (one at a time, 25 s hard timeout).

CLI:  python main.py serve            (uvicorn, single worker)
      python main.py unpause <PSID>   (resume bot replies for a user)
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import hmac
import json
import logging
import re
import time
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Annotated, Any, AsyncIterator, Dict, List, Set

from fastapi import FastAPI, HTTPException, Query, Request, Response
from fastapi.responses import FileResponse, HTMLResponse, PlainTextResponse

import config
import db
import events
import faq
import llm
import messenger
import notify
import orders

config.setup_logging()
config.warn_missing_config()

hdl = logging.getLogger("main")

START_TS = time.monotonic()

# Handoff keywords (case-insensitive, word boundaries).
HANDOFF_RE = re.compile(
    "|".join(
        [
            r"\bcomplaints?\b",
            r"\bcomplains?\b",
            r"\breklamos?\b",
            r"\brefunds?\b",
            r"\ballerg(?:y|ies)\b",
            r"\ballergic\b",
            r"\bbulk\s+orders?\b",
            r"\bhuman\b",
            r"\bstaff\b",
            r"\btao\b",
        ]
    ),
    re.IGNORECASE,
)

# Order intent: starts the deterministic order-intake form. Checked AFTER the
# handoff regex so "bulk order" (a complaint path) still wins. "delivery"
# questions do NOT match — only explicit ordering words do.
ORDER_INTENT_RE = re.compile(
    "|".join(
        [
            r"\borders?\b",
            r"\bpabili\b",
            r"\bumorder\b",
            r"\bpa[-\s]?order\b",
            r"\bpa[-\s]?deliver\b",
        ]
    ),
    re.IGNORECASE,
)

_bg_tasks: Set[asyncio.Task[None]] = set()


@asynccontextmanager
async def lifespan(_app: FastAPI) -> AsyncIterator[None]:
    await db.connect(config.settings.db_path)
    hdl.info(
        "startup",
        extra={"db_path": config.settings.db_path, "faq_path": config.settings.faq_path},
    )
    task = asyncio.create_task(_cleanup_loop())
    try:
        yield
    finally:
        task.cancel()
        await messenger.aclose()
        await db.close()
        hdl.info("shutdown")


app = FastAPI(title="Erlbrew Café Messenger bot", version="1.0.0", lifespan=lifespan)


async def _cleanup_loop() -> None:
    """Periodically drop stale dedupe rows, history, rate events and forms."""
    while True:
        try:
            await db.cleanup(
                seen_ttl_sec=24 * 3600,
                history_ttl_sec=config.settings.llm_history_ttl_sec,
                form_ttl_sec=config.settings.event_form_ttl_sec,
            )
        except asyncio.CancelledError:
            raise
        except Exception:
            hdl.exception("cleanup failed")
        await asyncio.sleep(config.settings.cleanup_interval_sec)


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------
@app.get("/webhook", response_class=PlainTextResponse)
async def webhook_verify(
    hub_mode: Annotated[str, Query(alias="hub.mode")] = "",
    hub_verify_token: Annotated[str, Query(alias="hub.verify_token")] = "",
    hub_challenge: Annotated[str, Query(alias="hub.challenge")] = "",
) -> PlainTextResponse:
    expected = config.settings.verify_token
    if (
        hub_mode == "subscribe"
        and expected
        and hmac.compare_digest(hub_verify_token.encode(), expected.encode())
    ):
        return PlainTextResponse(hub_challenge)
    return PlainTextResponse("Forbidden", status_code=403)


@app.post("/webhook", response_class=PlainTextResponse)
async def webhook_receive(request: Request) -> PlainTextResponse:
    body = await request.body()
    if not _signature_ok(body, request.headers.get("X-Hub-Signature-256", "")):
        hdl.warning("webhook rejected: bad signature")
        return PlainTextResponse("Invalid signature", status_code=403)
    try:
        payload = json.loads(body)
    except ValueError:
        hdl.warning("webhook: malformed JSON ignored (200 returned to Meta)")
        return PlainTextResponse("EVENT_RECEIVED")
    if not isinstance(payload, dict) or payload.get("object") != "page":
        return PlainTextResponse("EVENT_RECEIVED")
    for entry in payload.get("entry", []) or []:
        for event in (entry.get("messaging") or []):
            task = asyncio.create_task(process_event(event))
            _bg_tasks.add(task)
            task.add_done_callback(_bg_tasks.discard)
    return PlainTextResponse("EVENT_RECEIVED")


def _signature_ok(body: bytes, header: str) -> bool:
    secret = config.settings.app_secret
    if not secret:
        hdl.error("APP_SECRET not set: rejecting webhook POST (fail closed)")
        return False
    expected = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    provided = (header or "").strip().lower()
    if not provided.startswith("sha256="):
        return False
    return hmac.compare_digest(expected, provided)


@app.get("/health")
async def health() -> Dict[str, Any]:
    return {
        "ok": True,
        "uptime_s": int(time.monotonic() - START_TS),
        "db": db.connected(),
        "model": config.settings.ollama_model,
    }


_PRIVACY_FALLBACK = """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Erlbrew Cafe - Privacy Policy</title></head>
<body><main><h1>Erlbrew Cafe - Privacy Policy</h1>
<p>We collect only the information needed to answer your messages:
your Messenger page-scoped ID, your display name, and the text of your
messages. We never sell or share this data, and we never use it for
advertising.</p><p>[REPLACE data/privacy.html with the owner's own version.]</p>
</main></body></html>"""


@app.get("/privacy", response_class=HTMLResponse)
async def privacy() -> Response:
    """Public privacy policy. Meta requires this URL before an app can go Live.

    Served from data/privacy.html (owner-editable, no restart needed); the
    fallback below only exists so the route never 404s.
    """
    path = Path("data/privacy.html")
    try:
        if path.is_file():
            return FileResponse(path, media_type="text/html")
    except OSError:
        pass
    return HTMLResponse(_PRIVACY_FALLBACK)


@app.post("/admin/unpause/{psid}")
async def admin_unpause(psid: str, request: Request) -> Dict[str, Any]:
    token = request.headers.get("X-Admin-Token", "")
    expected = config.settings.admin_token
    if not expected or not token or not hmac.compare_digest(token.encode(), expected.encode()):
        raise HTTPException(status_code=403, detail="invalid admin token")
    ok = await db.unpause_user(psid)
    hdl.info("admin unpause", extra={"psid": psid})
    return {"ok": ok, "psid": psid}


@app.get("/admin/orders")
async def admin_orders(request: Request, limit: int = Query(50, ge=1, le=500)) -> Dict[str, Any]:
    """Newest intake orders first — backup view when Telegram isn't configured."""
    token = request.headers.get("X-Admin-Token", "")
    expected = config.settings.admin_token
    if not expected or not token or not hmac.compare_digest(token.encode(), expected.encode()):
        raise HTTPException(status_code=403, detail="invalid admin token")
    return {"ok": True, "orders": await db.list_orders(limit)}


# ---------------------------------------------------------------------------
# Event processing (background, never crashes the worker)
# ---------------------------------------------------------------------------
# One lock per customer: Meta can (and does) deliver a customer's burst of
# back-to-back messages as concurrent webhook POSTs. Form state machines,
# rate counters and the profile-name race all require STRICT per-psid order,
# so each customer's events are serialized across each other while different
# customers still run in parallel.
_PSID_LOCKS: Dict[str, asyncio.Lock] = {}


async def process_event(event: Dict[str, Any]) -> None:
    psid = str(((event.get("sender") or {}).get("id") or "")).strip()
    try:
        if not psid:
            return
        lock = _PSID_LOCKS.setdefault(psid, asyncio.Lock())
        async with lock:
            if "postback" in event:
                await _handle_postback(psid, dict(event.get("postback") or {}))
            elif isinstance(event.get("message"), dict):
                message = dict(event["message"])
                if message.get("is_echo"):
                    # A page-side (staff) message. The bot stays silent on those,
                    # but the first staff reply ALSO resumes the bot for that
                    # customer — that's how team mode ends after handoff/orders.
                    recipient = str(((event.get("recipient") or {}).get("id") or "")).strip()
                    if recipient and await db.get_paused(recipient):
                        if await db.unpause_user(recipient):
                            hdl.info("team replied; bot resumed", extra={"psid": recipient})
                    return
                await _handle_message(psid, message)
            # delivery / read / referral events are intentionally ignored
    except Exception:
        hdl.exception("event processing failed", extra={"psid": psid})
    finally:
        messenger.stop_typing(psid)


async def _handle_postback(psid: str, postback: Dict[str, Any]) -> None:
    payload = str(postback.get("payload") or "").strip()
    await db.upsert_user(psid)
    if await db.get_paused(psid):
        hdl.info("paused user postback ignored", extra={"psid": psid})
        return
    await messenger.send_typing(psid, "typing_on")
    await route_payload(psid, payload)


async def _handle_message(psid: str, message: Dict[str, Any]) -> None:
    mid = str(message.get("mid") or "")
    if mid and not await db.mark_message_seen(mid):
        hdl.info("duplicate message ignored", extra={"psid": psid, "mid": mid})
        return

    text = str(message.get("text") or "").strip()
    quick_payload = str(((message.get("quick_reply") or {}).get("payload")) or "").strip()
    attachments = list(message.get("attachments") or [])

    # 1) rate limit (15 msgs / 10 min) — one polite message, then ignore
    await db.note_rate_event(psid)
    over = await db.count_rate_events(psid, config.settings.rate_limit_window_sec) > (
        config.settings.rate_limit_max
    )
    if over:
        once = await db.pop_notice_once(
            psid, config.RATE_NOTICE_KIND, config.settings.rate_limit_window_sec
        )
        if once:
            hdl.info("rate limit notice sent", extra={"psid": psid})
            await messenger.send_text(psid, config.RATE_LIMITED_TEXT)
        return

    # 1b) max text length (500 chars) — one polite message, then ignore
    if len(text) > config.settings.max_text_chars:
        once = await db.pop_notice_once(
            psid, config.LEN_NOTICE_KIND, config.settings.rate_limit_window_sec
        )
        if once:
            hdl.info("long message notice sent", extra={"psid": psid, "len": len(text)})
            await messenger.send_text(psid, config.TOO_LONG_TEXT)
        return

    # 2) paused users get no replies
    await db.upsert_user(psid)
    if await db.get_paused(psid):
        hdl.info("paused user message ignored", extra={"psid": psid})
        return

    # Best-effort profile name for nicer Telegram notifications (first contact).
    if not await db.get_user_name(psid):
        name = await messenger.get_profile_name(psid)
        if name:
            await db.set_user_name(psid, name)

    await messenger.send_typing(psid, "typing_on")

    # 3) payloads (quick replies; postbacks were handled separately)
    if quick_payload and await route_payload(psid, quick_payload):
        return

    # Non-text messages (images/stickers): ask the customer to type instead.
    if not text:
        if attachments:
            row = await db.get_form(psid)
            step = str(row[0]) if row else ""
            if step.startswith("order_"):
                reply = await orders.current_question(psid)
            elif row is not None:
                reply = await events.current_question(psid)
            else:
                reply = None
            if reply is not None:
                await _send_reply(psid, reply)
            else:
                await messenger.send_text(psid, config.NON_TEXT_TEXT)
        return

    # 4) active event form continues (spec order: form before handoff keywords)
    if await events.has_active_form(psid):
        row = await db.get_form(psid)
        step = str(row[0]) if row else ""
        handler = orders.handle_input if step.startswith("order_") else events.handle_input
        reply = await handler(psid, text)
        if reply is not None:
            await _send_reply(psid, reply)
        return

    # 5) handoff keywords
    if HANDOFF_RE.search(text):
        await messenger.send_text(psid, config.HANDOFF_TEXT)
        await db.set_paused(psid, True)
        await db.delete_form(psid)
        name = await db.get_user_name(psid)
        await notify.notify_handoff(psid, name, text)
        hdl.info("handoff to team", extra={"psid": psid})
        return

    # 5b) order intent starts the order intake form (AFTER handoff so that
    #     e.g. "bulk order" complaints still go straight to the team)
    if ORDER_INTENT_RE.search(text):
        reply = await orders.start_form(psid)
        hdl.info("order form started", extra={"psid": psid})
        await _send_reply(psid, reply)
        return

    # 6) keyword/regex FAQ match
    entry = faq.text_entry(text)
    if entry is not None:
        await _send_entry(psid, entry)
        return

    # 7) LLM fallback
    answer = await llm.generate(psid, text)
    if answer is None:
        await messenger.send_text(psid, config.LLM_FALLBACK_TEXT)
    else:
        await messenger.send_text(psid, answer)


async def route_payload(psid: str, payload: str) -> bool:
    """Handle a postback / quick-reply payload. True when fully handled."""
    p = (payload or "").strip().upper()
    if not p:
        return False

    if p in ("ORD_OK", "ORD_EDIT"):
        # Confirm-step quick replies: route through the order form's handler
        # so the same logic applies to quick replies and typed answers.
        row = await db.get_form(psid)
        if row is not None and str(row[0]) == "order_confirm":
            reply = await orders.handle_input(psid, "tama" if p == "ORD_OK" else "edit")
            if reply is not None:
                await _send_reply(psid, reply)
        return True

    if p == "ORDER_START":
        row = await db.get_form(psid)
        step = str(row[0]) if row else ""
        if step.startswith("order_"):
            reply = await orders.current_question(psid)
        elif row is not None:
            reply = await events.current_question(psid)
        else:
            reply = await orders.start_form(psid)
        if reply is not None:
            await _send_reply(psid, reply)
        return True

    if p == "EVT_START":
        if await events.has_active_form(psid):
            reply = await events.current_question(psid)
        else:
            reply = await events.start_form(psid)
        if reply is not None:
            await _send_reply(psid, reply)
        return True

    if p.startswith("EVT_TYPE:"):
        # Form's own quick reply: only meaningful inside an active form.
        if await events.has_active_form(psid):
            reply = await events.handle_input(psid, p)
            if reply is not None:
                await _send_reply(psid, reply)
        return True

    entry = faq.payload_entry(p)
    if entry is not None:
        await _send_entry(psid, entry)
        return True

    # Unknown payload -> friendly reset message.
    help_entry = faq.entry_by_id("fallback_help")
    if help_entry is not None:
        await _send_entry(psid, help_entry)
    else:
        await messenger.send_text(psid, config.NON_TEXT_TEXT)
    return True


async def _send_reply(psid: str, reply: events.Reply) -> None:
    if reply.quick_replies:
        await messenger.send_quick_replies(
            psid, reply.text, [(t, p) for (t, p) in reply.quick_replies]
        )
    else:
        await messenger.send_text(psid, reply.text)


async def _send_entry(psid: str, entry: faq.Entry) -> None:
    hdl.info("faq answered", extra={"psid": psid, "entry": entry.id})
    qrs: List[tuple[str, str]] = [qr.as_pair() for qr in entry.quick_replies]
    if qrs:
        await messenger.send_quick_replies(psid, entry.answer, qrs)
    else:
        await messenger.send_text(psid, entry.answer)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------
def main() -> None:
    parser = argparse.ArgumentParser(prog="erlbrew-bot")
    sub = parser.add_subparsers(dest="cmd", required=True)
    serve_p = sub.add_parser("serve", help="run the webhook service (uvicorn)")
    serve_p.add_argument("--host", default="0.0.0.0")
    serve_p.add_argument("--port", type=int, default=config.settings.port)
    unpause_p = sub.add_parser("unpause", help="resume bot replies for one PSID")
    unpause_p.add_argument("psid", help="page-scoped user id (PSID)")
    args = parser.parse_args()

    if args.cmd == "serve":
        import uvicorn

        uvicorn.run(app, host=args.host, port=args.port, log_config=None, access_log=False)
        return

    async def _unpause() -> None:
        await db.connect(config.settings.db_path)
        ok = await db.unpause_user(args.psid)
        hdl.info("unpaused", extra={"psid": args.psid, "updated": ok})
        await messenger.aclose()
        await db.close()

    asyncio.run(_unpause())


if __name__ == "__main__":
    main()
