# Erlbrew Café — Facebook Messenger Chatbot

A 100%-free Messenger bot for **Erlbrew Café (Kawit, Cavite, Philippines)**,
for customers coming from click-to-Messenger ads about **DINE-IN** and
**EVENTS**.

Design principle: **rule-based first, LLM fallback.** Static YAML answers do
the heavy lifting; a tiny local model (Ollama, ~2B, CPU-only) only answers
free-text questions it can't match. All "AI" is free software running on your
own hardware — no paid APIs, no paid libraries.

```
Customer (Messenger)
        │  webhook (HTTPS)
        ▼
Cloudflare Tunnel ──► this service (FastAPI, Docker container)
        │                    │ rule-based answers from faq.yaml
        │                    ├─► Ollama (same host or LAN box) — fallback only
        │                    └─► Telegram notifications (leads / handoffs / failures)
        └── replies via Graph API send (24h standard messaging window)
```

## Feature checklist

- Meta webhook verification + **X-Hub-Signature-256** (HMAC-SHA256, constant-time
  compare; 403 on mismatch). 200 is returned immediately; processing happens in a
  background task.
- Echo filtering, **dedupe by message id** (SQLite), per-user rate limit
  (15 msgs / 10 min) and 500-char cap — one polite notice, then silence.
- Paused users (human handoff) get **no automatic replies** at all.
- Postbacks & quick replies (`MENU`, `HOURS`, `PROMO`, `EVENTS`, `GET_STARTED`,
  `EVT_START`) answered from `faq.yaml`.
- **Event booking form** — deterministic state machine (not LLM-driven):
  type → date/time → guests → name → contact → done. Quick replies for the
  event type, validation for guests & lenient PH mobile format, `cancel`
  anytime, leads saved to SQLite + Telegram notification, reply promise of
  `EVENT_REPLY_HOURS`. **The bot never confirms availability, dates or prices.**
- **Order intake form with live POS pricing** — deterministic state machine:
  items → priced receipt → confirm → name → address → mobile → done.
  Items are matched fuzzily (difflib, ≥ 0.75) against the café's real
  `menu_items` table in the POS MySQL database (`POS_MYSQL_URL`), with
  cached reads (60 s TTL) and variant awareness (sizes in the name or the
  description become "hot 8oz / iced 16oz" alternates). The receipt's total
  is an estimate — the team still confirms the final total, payment and
  schedule in the live chat. When the POS DB is unreachable the form
  degrades to plain free-text items; **the bot never invents a price**.
  Completed orders are saved to SQLite + Telegram ping; per-PSID webhook
  events are serialized so burst messages can't race the form; abandoned
  forms self-destruct after 2 h; the bot pauses after each order and
  auto-resumes when a page admin replies.
- Handoff keywords (complaint, refund, allergy, allergic, bulk order, human,
  staff, tao, reklamo) → polite message, pause the user, Telegram alert.
- Keyword/regex FAQ matching (menu, price, hours, location, wifi, promo,
  delivery, pickup, parking, payment).
- LLM fallback: one call at a time (`asyncio.Semaphore(1)`), 25 s hard timeout,
  `typing_on` re-sent every ~10 s while waiting, last-6-message history
  (expired after 24 h), static Taglish fallback + Telegram alert on failure.
- `typing_on` sent before every processing; images/stickers get one short
  "please type your question" reply.
- `GET /health`, `POST /admin/unpause/{psid}` (via `X-Admin-Token`), CLI
  `python main.py unpause <PSID>`, and `GET /admin/orders` (token-protected,
  newest orders first with the POS receipt lines + estimated total).
- **Live-editable content**: `data/faq.yaml` (Taglish static answers) and
  `data/knowledge.md` (LLM system prompt ≤ ~400 words) re-read on mtime change.

## Project layout

| File | Purpose |
|---|---|
| `main.py` | FastAPI app, webhook endpoints, message pipeline, CLI (`serve`, `unpause`) |
| `config.py` | env-driven settings, JSON logging, canned messages |
| `db.py` | SQLite (aiosqlite): dedupe, rate limit, forms, leads, history, pauses |
| `messenger.py` | Graph API send side + typing keeper (Bearer header — tokens never in URLs/logs) |
| `faq.py` | hot-reloading FAQ engine + shared `HotFile` |
| `events.py` | event booking state machine |
| `orders.py` | order intake state machine (items → priced receipt → confirm → contact) |
| `menu.py` | live POS menu reader (read-only MySQL via aiomysql, TTL cache, fuzzy matcher) |
| `llm.py` | the single Ollama `generate()` |
| `notify.py` | Telegram notifications |
| `setup_profile.py` | one-time Messenger profile setup (get_started, ice breakers, persistent menu) |
| `send_test_webhook.py` | signed fake webhook for local testing |
| `faq.yaml`, `knowledge.md` | owner-editable content (placeholders included) |
| `tests/` | pytest suite — everything mocked, hits no real service |

---

## Quick start (local)

```bash
cp .env.example .env          # fill in values (see below)
pip install -r requirements-dev.txt

# 1) run the bot
python main.py serve          # http://localhost:8000

# 2) in another terminal, test without Meta:
APP_SECRET=whatever_you_used python send_test_webhook.py --text "What are your hours?"

# 3) run the tests
pytest
```

`python main.py serve --port 9000` overrides the port; `python main.py unpause
<PSID>` resumes a paused user straight from the CLI.

### Docker

```bash
cp .env.example .env && nano .env
mkdir -p data && cp faq.yaml knowledge.md data/  # live-editable copies
docker compose up -d --build
```

Runs as a non-root container, single uvicorn worker, `cpus: 0.60` / `memory:
256M` limits, healthcheck on `/health`, logs capped (5 MB × 2). Suited to a
small shared server (e.g. Lenovo M92p Tiny) managed from Portainer: just
"Deploy stack" with this compose file, or build once and point the stack at
the image. `data/` is bind-mounted so `faq.yaml`, `knowledge.md` and the
SQLite DB survive rebuilds and are editable live.

> `OLLAMA_BASE_URL` defaults to `http://host.docker.internal:11434`, and compose
> adds `extra_hosts: host.docker.internal:host-gateway` so a container can
> reach an Ollama running on the Docker host itself.

---

## Meta (Facebook) setup — full walkthrough

1. **Create the app.** Go to <https://developers.facebook.com>, *My Apps →
   Create App → Business type*. After creation, add the product **Messenger**
   (and note the *App ID*). You must be an admin of the Facebook Page.

2. **App Secret.** *App Settings → Basic → App secret*. Copy it into
   `APP_SECRET` in `.env`. It signs every webhook payload.

3. **Page Access Token.** *Messenger settings → Access Tokens → your page →
   Generate Token* (or use a System User token with `pages_show_list` +
   `pages_messaging`). Paste into `PAGE_ACCESS_TOKEN`.

4. **Verify token.** Invent a random string for `VERIFY_TOKEN` in `.env`.

5. **Webhook.** *Messenger platform settings → Webhooks*. Use:
   - Callback URL: `https://your-tunnel-host/webhook` (see Cloudflare Tunnel below)
   - Verify token: same `VERIFY_TOKEN`
   Then in **Webhook fields → Edit** subscribe the *page* subscription to:
   - `messages`
   - `messaging_postbacks`
   (You may also tick `messaging_referrals`, `messaging_optins` for ads; the
   bot ignores delivery/read events anyway.)

6. **Profile config (one-time).** With the page token set in your env:

   ```bash
   PAGE_ACCESS_TOKEN=EAAx... python setup_profile.py
   ```

   This sets the **Get Started** button, 4 **Ice Breakers**, and a
   **persistent menu** (Menu / Hours & Location / Promo / Events + Start over)
   — all wired to the payload names in `faq.yaml`. `--get` shows what is set,
   `--unset` clears them. Graph version comes from `GRAPH_VERSION` (default v23.0).

7. **App review reality check.** While the app is **in development mode**, only
   app admins/developers/testers can chat with the bot. Before it can answer
   the general public, submit for **App Review** with the `pages_messaging`
   (Advanced Access) permission and a screen recording of a live chat
   conversation. Until then: add yourself/your team as testers or admins, or
   the bot will silently ignore strangers.

---

## Cloudflare Tunnel (free public HTTPS)

On the same Ubuntu host (server-as-a-service + Cloudflare free plan):

```bash
curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 \
  -o /usr/local/bin/cloudflared && chmod +x /usr/local/bin/cloudflared
cloudflared tunnel login
cloudflared tunnel create erlbrew
cloudflared tunnel route dns erlbrew bot.erlbrew.example.com
cloudflared tunnel run --url http://localhost:8000 erlbrew   # install as a systemd service for real use
```

`cloudflared` (systemd) keeps the tunnel up with no extra components. The
compose file binds the bot to `127.0.0.1:8000` so nothing else is exposed.

> **Testing tip:** `cloudflared tunnel --url http://localhost:8000` gives you a
> temporary trycloudflare.com URL for Meta's webhook verification.

---

## Ollama (free, local) — bundled sidecar, zero setup

`docker-compose.yml` ships an `ollama` service next to the bot, so the LLM
fallback works out of the box — no Ollama install on the host needed:

```bash
docker compose up -d                        # starts ollama + bot
docker exec ollama ollama pull qwen3.5:2b   # once; ~2.7 GB stored in the ollama_models volume
```

- The bot reaches it at `http://ollama:11434` over the compose network
  (`.env`: `OLLAMA_BASE_URL=http://ollama:11434`; loopback port
  `127.0.0.1:11435` on the host exists only for manual `curl` tests).
- Models persist across container recreations in the named volume
  `ollama_models`.
- The host's own Ollama systemd service is left untouched. If you ever prefer
  running it on the host instead (it currently only listens on 127.0.0.1:
  `sudo systemctl edit ollama` → `Environment="OLLAMA_HOST=0.0.0.0:11434"`),
  delete the `ollama` service + the `ollama_models` volume block from
  `docker-compose.yml` and switch `.env` back to `http://<server-LAN-IP>:11434`.

The bot calls `POST {OLLAMA_BASE_URL}/api/chat` with:

```json
{"model": "qwen3.5:2b", "stream": false, "think": false,
 "keep_alive": "30m",
 "options": {"num_predict": 120, "num_ctx": 2048, "temperature": 0.2}}
```

- Only ONE generation at a time; extra questions queue and time out after
  `LLM_TIMEOUT_SEC` (25 s) into the static Taglish fallback
  ("Sandali lang po, ipapasa ko sa team namin.") + Telegram alert.
- Per-user history: last 6 messages, dropped after 24 h of inactivity.
- The system prompt lives in `knowledge.md` (**≤ ~400 words**; keep INFO to
  facts you actually want the bot to say — never real-time stock or prices
  you don't want printed).

### Ollama on another LAN machine

On the (faster) machine: set `OLLAMA_HOST=0.0.0.0`, then `ollama serve`, and
restrict listening to the LAN (firewall the server's IP + port 11434).

On the server: `OLLAMA_BASE_URL=http://192.168.x.y:11434` in `.env`.
`host.docker.internal` + `extra_hosts: host-gateway` (already set in compose)
covers `http://host.docker.internal:11434`.

> Rule of thumb: a weak CPU serves a qwen ~2B model at roughly 3-5 tokens/s
> with q4 quantization — that's why every common question is answered by the
> rule-based layer first and the LLM is only a fallback.

---

## Telegram notifications

1. Talk to [@BotFather](https://t.me/BotFather) → `/newbot` → copy the token
   into `TELEGRAM_BOT_TOKEN`.
2. Send **any message to the new bot once** (e.g. "hi"), then open:

   ```
   https://api.telegram.org/bot<TOKEN>/getUpdates
   ```

   …and copy `"chat":{"id": ...}` into `TELEGRAM_CHAT_ID` (use a group chat and
   add the bot to the group for the whole team; super-group ids look like
   `-100…`).
3. The bot then pings the team for: **new event leads**, **human handoffs**,
   and **LLM timeouts/failures**. No secrets are ever included in a message
   (only names/PSIDs/snippets).

---

## Configuration reference (`.env`)

| Var | Meaning |
|---|---|
| `APP_SECRET` | signs/verifies webhook payloads (required) |
| `VERIFY_TOKEN` | webhook challenge handshake (required) |
| `PAGE_ACCESS_TOKEN` | page token for sends (required) |
| `GRAPH_VERSION` | default `v23.0` |
| `ADMIN_TOKEN` | `X-Admin-Token` for `POST /admin/unpause/{psid}` |
| `OLLAMA_BASE_URL` / `OLLAMA_MODEL` | e.g. `http://host.docker.internal:11434`, `qwen3.5:2b` (`OLLAMA_URL` still accepted) |
| `OLLAMA_THINK` / `OLLAMA_NUM_PREDICT` / `OLLAMA_NUM_CTX` / `OLLAMA_TEMPERATURE` / `OLLAMA_KEEP_ALIVE` | Ollama tuning, CPU-friendly defaults: `false / 120 / 2048 / 0.2 / 30m` |
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | notifications |
| `EVENT_REPLY_HOURS` | promised team-reply time on completed bookings (24) |
| `DB_PATH` | SQLite `data/erlbrew.db` |
| `FAQ_PATH` / `KNOWLEDGE_PATH` | live-editable files (`data/…`) |
| `RATE_LIMIT_MAX` / `RATE_LIMIT_WINDOW_SEC` | 15 / 600 |
| `MAX_TEXT_CHARS` | per-message input cap (500) |
| `LLM_TIMEOUT_SEC` | hard fallback timeout (25) |
| `PORT` / `LOG_LEVEL` | server basics |

## Editable content (no restart / no rebuild)

- **`data/faq.yaml`** — static Taglish answers for menu, hours+location,
  promo, wifi, delivery, pickup, parking, payments, events + the welcome
  message. Replace all `[PLACEHOLDER: …]` blocks. Entries support
  `keywords` (whole words), `patterns` (substrings), `regex` (careful) and
  optional `quick_replies`.
- **`data/knowledge.md`** — the LLM's system prompt (≤ ~400 words, English —
  keep it tight so the tiny model stays on track).
- Both files are re-read the moment their mtime changes; wrong YAML only logs
  an error (last good copy keeps being used).
- Event form wording and canned texts (rate limit, handoff, etc.) live in
  `events.py` / `config.py` and need a container restart.

## Ops cheat-sheet

```bash
# pause state lives in SQLite; if a customer had a bad handoff later:
curl -X POST https://bot.example.com/admin/unpause/PSID_123 \
     -H "X-Admin-Token: $ADMIN_TOKEN"
# ...or inside the container:
python main.py unpause PSID_123

# full signed webhook test (uses APP_SECRET):
APP_SECRET=... python send_test_webhook.py --psid TEST123 --text "I need a refund"
APP_SECRET=... python send_test_webhook.py --postback EVENTS
APP_SECRET=... python send_test_webhook.py --quick MENU --psid TEST123
APP_SECRET=... python send_test_webhook.py --long 600          # 500-char cap test

# leads so far (they are also Telegram-alerted):
sqlite3 data/erlbrew.db 'SELECT * FROM event_leads;'
```

## Tests

```bash
pip install -r requirements-dev.txt
pytest            # 56 tests: signature, dedupe, rate limit, FAQ matching,
                  # handoff keywords, event form flow (invalid + cancel),
                  # pipeline order, LLM payload/timeout/semaphore, Telegram
```

Everything is mocked (`httpx.MockTransport` + patched senders): no Meta,
Ollama or Telegram traffic happens in tests.

## Limits & notes

- Uses Messenger's **standard messaging window** only: the bot replies within
  24 h of receiving a customer message. No message tags, no broadcasts, no
  re-marketing blasts.
- Never says "yes, your event is confirmed": even the final form message
  explicitly states the team will confirm availability & price.
- Logs are structured JSON but contain **no tokens or secrets**; Graph auth
  uses a Bearer header so the token never appears in a URL either.
