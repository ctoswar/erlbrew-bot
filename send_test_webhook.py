"""Send a correctly signed fake Messenger webhook to a local /webhook.

Lets you test the bot end-to-end (signature -> dedupe -> pipeline) without
Meta. Requires the SAME APP_SECRET as the running service.

Examples:
  python send_test_webhook.py --text "What are your hours?"
  python send_test_webhook.py --postback MENU
  python send_test_webhook.py --quick MENU
  python send_test_webhook.py --text "I need a refund" --psid 123
  APP_SECRET=secret python send_test_webhook.py --long 600

Defaults: APP_SECRET from env, --url http://localhost:8000/webhook,
--psid TEST_PSID_1. Never prints or logs the secret.
"""
from __future__ import annotations

import argparse
import hmac
import hashlib
import json
import os
import secrets
import sys
import time
from typing import Any, Dict

try:
    import httpx
except ImportError:  # pragma: no cover - dev dependency
    print("ERROR: httpx is required (pip install -r requirements-dev.txt)")
    sys.exit(2)


def build_payload(args: argparse.Namespace) -> Dict[str, Any]:
    sender = {"id": args.psid}
    recipient = {"id": os.getenv("FB_PAGE_ID", "PAGE_ID")}
    ts = int(time.time() * 1000)

    if args.postback:
        messaging_event: Dict[str, Any] = {
            "sender": sender,
            "recipient": recipient,
            "timestamp": ts,
            "postback": {"title": args.title or args.postback, "payload": args.postback},
        }
    else:
        message: Dict[str, Any] = {"mid": f"mid.{ts}:{secrets.token_hex(8)}", "text": args.text or ""}
        if args.quick:
            message["text"] = message["text"] or args.quick
            message["quick_reply"] = {"payload": args.quick}
        if args.attachment:
            message["text"] = ""
            message["attachments"] = [{"type": "image", "payload": {"sticker_id": 0}}]
        messaging_event = {
            "sender": sender,
            "recipient": recipient,
            "timestamp": ts,
            "message": message,
        }

    return {
        "object": "page",
        "entry": [
            {"id": recipient["id"], "time": ts, "messaging": [messaging_event]}
        ],
    }


def sign(app_secret: str, body: bytes) -> str:
    digest = hmac.new(app_secret.encode(), body, hashlib.sha256).hexdigest()
    return f"sha256={digest}"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://localhost:8000/webhook")
    parser.add_argument("--psid", default="TEST_PSID_1")
    parser.add_argument("--text", default=None, help="message text to send")
    parser.add_argument("--postback", default=None, help="postback payload (e.g. MENU)")
    parser.add_argument("--quick", default=None, help="quick-reply payload (e.g. MENU)")
    parser.add_argument("--title", default=None, help="postback title")
    parser.add_argument("--attachment", action="store_true", help="send a fake image")
    parser.add_argument("--long", dest="long_text", type=int, default=0,
                        help="send a text message of N characters (>500 triggers cap)")
    parser.add_argument("--secret", default=os.getenv("APP_SECRET", ""),
                        help="APP_SECRET (defaults to the APP_SECRET env var)")
    args = parser.parse_args()

    if not args.secret:
        print("ERROR: APP_SECRET missing (--secret or env)")
        sys.exit(2)
    if args.long_text:
        args.text = "x" * args.long_text
    if not (args.text or args.postback or args.quick or args.attachment):
        args.text = "Hi!"

    payload = build_payload(args)
    body = json.dumps(payload, separators=(",", ":")).encode()

    headers = {"Content-Type": "application/json", "X-Hub-Signature-256": sign(args.secret, body)}
    try:
        resp = httpx.post(args.url, content=body, headers=headers, timeout=15.0)
    except httpx.HTTPError as exc:
        print(f"ERROR: request failed: {type(exc).__name__}: {exc}")
        sys.exit(1)
    print(f"HTTP {resp.status_code} -> {resp.text}")


if __name__ == "__main__":
    main()
