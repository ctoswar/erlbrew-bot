"""Webhook endpoint tests: Meta verification, X-Hub-Signature-256, admin auth."""
from __future__ import annotations

import json

import config

from conftest import sign_headers


async def test_get_webhook_challenge_ok(api):
    resp = await api.get(
        "/webhook",
        params={
            "hub.mode": "subscribe",
            "hub.verify_token": config.settings.verify_token,
            "hub.challenge": "CHALLENGE_123",
        },
    )
    assert resp.status_code == 200
    assert resp.text == "CHALLENGE_123"


async def test_get_webhook_wrong_token(api):
    resp = await api.get(
        "/webhook",
        params={"hub.mode": "subscribe", "hub.verify_token": "nope", "hub.challenge": "x"},
    )
    assert resp.status_code == 403


async def test_get_webhook_missing_token(api):
    resp = await api.get("/webhook", params={"hub.challenge": "x"})
    assert resp.status_code == 403


async def test_post_webhook_valid_signature(api, sends, tg):
    body = json.dumps(
        {"object": "page", "entry": [{"id": "PAGE", "messaging": []}]}
    ).encode()
    resp = await api.post("/webhook", content=body, headers=sign_headers(body, config.settings.app_secret))
    assert resp.status_code == 200
    assert resp.text == "EVENT_RECEIVED"
    assert sends.texts() == []  # nothing to process


async def test_post_webhook_invalid_signature(api, sends):
    body = json.dumps(
        {"object": "page", "entry": [{"id": "PAGE", "messaging": []}]}
    ).encode()
    resp = await api.post("/webhook", content=body, headers=sign_headers(body, "wrong_secret"))
    assert resp.status_code == 403


async def test_post_webhook_missing_header(api, sends):
    body = json.dumps(
        {"object": "page", "entry": [{"id": "PAGE", "messaging": []}]}
    ).encode()
    resp = await api.post("/webhook", content=body, headers={"Content-Type": "application/json"})
    assert resp.status_code == 403


async def test_post_webhook_bad_json_still_200(api):
    body = b"not json at all"
    resp = await api.post("/webhook", content=body, headers=sign_headers(body, config.settings.app_secret))
    assert resp.status_code == 200


async def test_post_webhook_wrong_object(api):
    body = json.dumps({"object": "whatsapp", "entry": []}).encode()
    resp = await api.post("/webhook", content=body, headers=sign_headers(body, config.settings.app_secret))
    assert resp.status_code == 200


async def test_health(api):
    resp = await api.get("/health")
    assert resp.status_code == 200
    data = resp.json()
    assert data["ok"] is True
    assert data["db"] is True


async def test_privacy(api):
    resp = await api.get("/privacy")
    assert resp.status_code == 200
    assert "privacy policy" in resp.text.lower()
    assert "Erlbrew" in resp.text


async def test_admin_unpause_requires_token(api):
    resp = await api.post("/admin/unpause/PSID_X")
    assert resp.status_code == 403
    resp = await api.post("/admin/unpause/PSID_X", headers={"X-Admin-Token": "wrong"})
    assert resp.status_code == 403


async def test_admin_unpause_with_token(api, sends, tg):
    resp = await api.post(
        "/admin/unpause/PSID_ADMIN", headers={"X-Admin-Token": config.settings.admin_token}
    )
    assert resp.status_code == 200
    assert resp.json() == {"ok": True, "psid": "PSID_ADMIN"}
