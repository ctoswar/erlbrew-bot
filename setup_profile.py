"""One-time Messenger profile setup for Erlbrew Café.

Configures (via POST /{GRAPH_VERSION}/me/messenger_profile):
  - get_started button payload: GET_STARTED
  - ice_breakers: Menu, Hours & Location, Promo, Events
  - persistent_menu: Menu / Hours & Location / Promo / Events + typing composer

Usage:
  python setup_profile.py            # set everything (idempotent)
  python setup_profile.py --get      # just show the current profile
  python setup_profile.py --unset    # clear get_started/ice_breakers/menu

Requires PAGE_ACCESS_TOKEN (+ GRAPH_VERSION, default v23.0) in the
environment / .env file. Never prints the token.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any, Dict

try:
    # Small convenience: load .env without adding a dependency.
    from dotenv import load_dotenv  # type: ignore
except ImportError:  # python-dotenv is optional for this script
    def load_dotenv(*_a: Any, **_k: Any) -> bool:
        return False

import httpx

ENV_CANDIDATES = [".env", "data/.env"]

GRAPH_BASE = "https://graph.facebook.com"

ICE_BREAKERS = [
    {"question": "Anong meron sa menu?", "payload": "MENU"},
    {"question": "Saan kayo at anong oras open?", "payload": "HOURS"},
    {"question": "May promos ba ngayon?", "payload": "PROMO"},
    {"question": "Pwede ba mag-book ng event?", "payload": "EVENTS"},
]

MENU = [
    {"title": "Menu", "type": "postback", "payload": "MENU"},
    {"title": "Hours & Location", "type": "postback", "payload": "HOURS"},
    {"title": "Promo", "type": "postback", "payload": "PROMO"},
    {"title": "Events", "type": "postback", "payload": "EVENTS"},
    {"title": "Order now 🛍️", "type": "postback", "payload": "ORDER_START"},
]


def _load_env() -> None:
    load_dotenv()
    # tiny fallback parser so the script also works without python-dotenv
    if os.getenv("PAGE_ACCESS_TOKEN"):
        return
    for cand in ENV_CANDIDATES:
        p = Path(cand)
        if not p.exists():
            continue
        for line in p.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            os.environ.setdefault(key.strip(), value.strip().strip("'\""))


def _client() -> httpx.Client:
    return httpx.Client(timeout=20.0)


def _url(version: str, op: str) -> str:
    return f"{GRAPH_BASE}/{version}/me/{op}"


def _headers(token: str) -> Dict[str, str]:
    # Bearer header keeps the token out of URLs / logs.
    return {"Authorization": f"Bearer {token}"}


def _print_result(label: str, data: dict) -> None:
    err = data.get("error")
    if err:
        print(f"[FAIL] {label}: {err.get('message', err)} (code {err.get('code', '?')})")
        sys.exit(1)
    print(f"[OK]   {label}: {json.dumps(data)}")


def set_profile(client: httpx.Client, token: str, version: str) -> None:
    body: Dict[str, Any] = {
        "get_started": {"payload": "GET_STARTED"},
        "ice_breakers": ICE_BREAKERS,
        "persistent_menu": [
            {
                "locale": "default",
                "composer_input_disabled": False,
                "call_to_actions": MENU,
            }
        ],
    }
    resp = client.post(_url(version, "messenger_profile"), json=body, headers=_headers(token))
    _print_result("set_profile (get_started + ice_breakers + persistent_menu)", _json(resp))


def get_profile(client: httpx.Client, token: str, version: str) -> None:
    resp = client.get(
        _url(version, "messenger_profile"),
        params={"fields": "get_started,ice_breakers,persistent_menu", "access_token": token},
    )
    _print_result("get_profile", _json(resp))


def unset_profile(client: httpx.Client, token: str, version: str) -> None:
    resp = client.delete(
        _url(version, "messenger_profile"),
        params={"fields": "[" + ",".join(["get_started", "ice_breakers", "persistent_menu"]) + "]"},
        headers=_headers(token),
    )
    _print_result("unset_profile", _json(resp))


def _json(resp: httpx.Response) -> Dict[str, Any]:
    try:
        data = resp.json()
    except ValueError:
        data = {"error": {"message": f"non-JSON response (HTTP {resp.status_code})"}}
    return data if isinstance(data, dict) else {"result": data}


def main() -> None:
    _load_env()
    parser = argparse.ArgumentParser(description="Erlbrew Café Messenger profile setup")
    parser.add_argument("--get", action="store_true", help="show current profile")
    parser.add_argument("--unset", action="store_true", help="clear configured items")
    args = parser.parse_args()

    token = os.getenv("PAGE_ACCESS_TOKEN", "").strip()
    version = os.getenv("GRAPH_VERSION", "v23.0").strip()
    if not token:
        print("ERROR: PAGE_ACCESS_TOKEN is not set (env or .env)")
        sys.exit(2)

    with _client() as client:
        if args.get:
            get_profile(client, token, version)
            return
        if args.unset:
            unset_profile(client, token, version)
            return
        set_profile(client, token, version)
        get_profile(client, token, version)


if __name__ == "__main__":
    main()
