# Erlbrew Café — Facebook Messenger Chatbot

A 100%-free Messenger bot for **Erlbrew Café (Kawit, Cavite, Philippines)**,
for customers coming from click-to-Messenger ads about **DINE-IN** and
**EVENTS**.

The bot is a single native **Rust** service. There is no Python runtime.

Design principle: **rule-based first, LLM fallback.** Static YAML answers do
the heavy lifting; a tiny local model (Ollama, ~2B, CPU-only) only answers
free-text questions it can't match. All "AI" is free software running on your
own hardware — no paid APIs, no paid libraries.

```
Customer (Messenger)
        │  webhook (HTTPS)
        ▼
Cloudflare Tunnel ──► this service (Rust/axum, Docker container)
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
- **Loyalty program**: "loyalty card", "stamps", "points", "rewards" → the bot points to the
  Erlbrew loyalty app (`https://app.erlbrewcafe.page/`). It never invents how many stamps are
  needed or what the reward is — the stamp policy lives in `LOYALTY_POLICY` in `src/topics.rs`
  and stays empty until the owner fills it in. Lost points, a missing stamp or an app problem
  are handed to the team (the customer is paused and Telegram is notified).
- **Private events**: "events", "birthday", "meeting", "photoshoot", "packages" → what the café
  hosts plus a pointer to the **Start booking** button. No prices, packages, capacity or
  availability are ever listed — the team sends those after the booking form.
- **Reservations (regular seating)**: "mag-reserve ng table", "may reservation ba?", "i-hold yung
  seats" → walk-in only, first come first served. No table is ever promised or held. This is
  checked *before* the booking intent, so "mag-reserve ng table" never opens the event form.
- Paused users (human handoff) get **no automatic replies** at all.
- **Acknowledgements & closings**: "thank you", "salamat", "ty", "ok", "okay", "sige", "noted",
  "got it", "alright", 👍 🙏 → **one** short warm Taglish line (rotating, one emoji max).
  The welcome message, the hours/WiFi intro and "How can I help you today?" are never repeated,
  and the LLM is not consulted. If a booking/order form is still open, the line also carries a
  gentle reminder of the next step ("Para sa order, isulat lang po ang items 👇").
- **Human takeover**: when a page admin replies from the Page inbox, the bot
  pauses that conversation (and cancels any half-finished form) so it never
  talks over the team. Resume with `POST /admin/unpause/{psid}`.
- Postbacks & quick replies (`MENU`, `HOURS`, `PROMO`, `EVENTS`, `GET_STARTED`,
  `EVT_START`) answered from `faq.yaml`.
- **Event booking form** — deterministic state machine (not LLM-driven):
  type → date/time → guests → name → contact → done. Quick replies for the
  event type, validation for guests & lenient PH mobile format, `cancel`
  anytime, leads saved to SQLite + Telegram notification, reply promise of
  `EVENT_REPLY_HOURS`. **The bot never confirms availability, dates or prices.**
- **Order intake form with live POS pricing** — deterministic state machine:
  items → priced receipt → confirm → name → address → mobile → done.
  Items are matched fuzzily against the café's real `menu_items` table in the
  POS MySQL database (`POS_MYSQL_URL`), with cached reads (60 s TTL) and
  variant awareness. The receipt total is an estimate — the team still
  confirms the final total, payment and schedule in the live chat. When the
  POS DB is unreachable the form degrades to plain free-text items; **the bot
  never invents a price**. Completed orders are saved to SQLite + Telegram.
- Handoff keywords (complaint, refund, allergy, allergic, bulk order, human,
  staff, tao, reklamo) → polite message, pause the user, Telegram alert.
- Keyword/regex FAQ matching (menu, price, hours, location, wifi, promo,
  delivery, pickup, parking, payment).
- LLM fallback with a hard timeout, static Taglish fallback, and Telegram
  alert on failure.
- `typing_on` sent before every processing; images/stickers get one short
  "please type your question" reply.
- `GET /health`, `POST /admin/unpause/{psid}` (via `X-Admin-Token`), and
  `GET /admin/orders` (token-protected, newest orders first).
- **Live-editable content**: `faq.yaml` (Taglish static answers) and
  `knowledge.md` (LLM system prompt ≤ ~400 words) re-read on mtime change.

## Project layout

| File | Purpose |
|---|---|
| `src/main.rs` | binary entrypoint, config wiring, graceful shutdown |
| `src/lib.rs` | library entrypoint used by the integration tests |
| `src/config.rs` | env-driven settings, JSON logging, canned messages |
| `src/db.rs` | SQLite (rusqlite): dedupe, rate limit, forms, leads, pauses |
| `src/messenger.rs` | Graph API send side + typing keeper |
| `src/faq.rs` | hot-reloading FAQ engine |
| `src/forms.rs` | shared form helpers (reply type, validation, cancel) |
| `src/acks.rs` | acknowledgement / closing detection + rotating closing lines |
| `src/topics.rs` | deterministic answers: loyalty, private events, reservations |
| `src/events.rs` | event booking state machine |
| `src/orders.rs` | order intake state machine |
| `src/menu.rs` | live POS menu reader (read-only MySQL, TTL cache, fuzzy matcher) |
| `src/pipeline.rs` | message pipeline (dedupe → rate limit → forms → FAQ → LLM) |
| `src/routes.rs` | axum routes (`/webhook`, `/health`, `/admin/*`, `/privacy`) |
| `src/llm.rs` | the single Ollama `generate()` call |
| `src/notify.rs` | Telegram notifications |
| `src/state.rs` | shared `App` state |
| `faq.yaml`, `knowledge.md` | owner-editable content (placeholders included) |
| `Cargo.toml`, `Cargo.lock` | Rust project configuration and lockfile |
| `Dockerfile` | multi-stage Rust build (builder + `debian:bookworm-slim`) |

## Build & test

Requirements: Rust 1.80+ and a C toolchain (`build-essential`, `pkg-config`).

```bash
cargo build --release      # target/release/erlbrew-bot
cargo test                 # unit + integration tests, all mocked
```

The tests cover signatures, dedupe, rate limiting, FAQ matching, handoff
keywords, both form flows (invalid input + cancel), pipeline order, webhook
parsing, HTTP endpoints, and Telegram payloads. No Meta, Ollama, MySQL or
Telegram traffic happens in tests.

## Quick start (local)

```bash
cp .env.example .env          # fill in values (see below)
set -a; . ./.env; set +a      # export the values (the binary does not parse .env)
cargo run --release           # http://localhost:8000
```

For a signed local webhook test, use `curl` with an HMAC computed from
`APP_SECRET`:

```bash
BODY='{"object":"page","entry":[{"messaging":[{"sender":{"id":"TEST123"},"message":{"mid":"m1","text":"What are your hours?"}}]}]}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$APP_SECRET" | awk '{print $2}')
curl -X POST http://localhost:8000/webhook \
  -H "Content-Type: application/json" \
  -H "X-Hub-Signature-256: sha256=$SIG" \
  --data "$BODY"
```

## Docker

```bash
cp .env.example .env && nano .env
mkdir -p data && cp faq.yaml knowledge.md data/ # live-editable copies
docker compose up -d --build
```

The image is built in two stages: `rust:1-bookworm` compiles the binary, then
`debian:bookworm-slim` runs it with only `ca-certificates`. Compose runs the
container as UID/GID `1000:1000` so the `./data` bind mount (SQLite + editable
files) is writable by the host user. Resource limits are `cpus: 0.60` /
`memory: 256M`, logs are capped (5 MB × 2), and the healthcheck calls
`/usr/local/bin/bot --health-check`.

`data/` is bind-mounted so `faq.yaml`, `knowledge.md`, `privacy.html` and the
SQLite DB survive rebuilds and are editable live.

An existing SQLite database created by the old Python bot is migrated
automatically on startup: renamed columns (`seen_at`, `last_sent`,
`created_at`) are fixed in place, and legacy `event_forms` / `event_leads`
rows are copied into `forms` / `leads`.

> `OLLAMA_BASE_URL` defaults to `http://host.docker.internal:11434`, and
> compose adds `extra_hosts: host.docker.internal:host-gateway` so a container
> can reach an Ollama running on the Docker host itself.

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

The bot calls `POST {OLLAMA_URL}/api/chat` with:

```json
{"model": "qwen3.5:2b", "stream": false, "think": false, "keep_alive": "24h",
 "messages": [{"role": "system", "content": "..."}, {"role": "user", "content": "..."}],
 "options": {"temperature": 0.2, "num_predict": 120, "num_ctx": 2048}}
```

- `think: false` is important on thinking-capable models: it skips the long
  reasoning pass and keeps warm answers around 1 s instead of 60 s+.
- The model is loaded once at startup (warm-up) and kept resident with
  `OLLAMA_KEEP_ALIVE` so the first customer does not pay the cold start.
- Requests time out after `LLM_TIMEOUT_SEC` (90 s) and fall back to the static
  Taglish message + Telegram alert. The startup warm-up has its own 5-minute
  timeout so a slow cold load cannot abort it.
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
compose file publishes port 8085; point the tunnel at the bot container.

> **Testing tip:** `cloudflared tunnel --url http://localhost:8000` gives you a
> temporary trycloudflare.com URL for Meta's webhook verification.

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

3. The bot then pings the team for: **new event leads**, **new orders**,
   **human handoffs**, and **LLM failures**. No secrets are ever included in a
   message (only names/PSIDs/snippets).

## Configuration reference (`.env`)

| Var | Meaning |
|---|---|
| `APP_SECRET` | signs/verifies webhook payloads (required) |
| `VERIFY_TOKEN` | webhook challenge handshake (required) |
| `PAGE_ACCESS_TOKEN` | page token for sends (required) |
| `GRAPH_API_VERSION` | default `v21.0` (`GRAPH_VERSION` also accepted) |
| `ADMIN_TOKEN` | `X-Admin-Token` for `POST /admin/unpause/{psid}` |
| `OLLAMA_BASE_URL` / `OLLAMA_MODEL` | e.g. `http://host.docker.internal:11434`, `qwen3.5:2b` (`OLLAMA_URL` also accepted) |
| `OLLAMA_THINK` / `OLLAMA_NUM_PREDICT` / `OLLAMA_NUM_CTX` / `OLLAMA_TEMPERATURE` / `OLLAMA_KEEP_ALIVE` | Ollama tuning: `false / 120 / 2048 / 0.2 / 24h` |
| `LLM_TIMEOUT_SEC` | hard fallback timeout (90) |
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | notifications |
| `EVENT_REPLY_HOURS` | promised team-reply time on completed bookings (24) |
| `DB_PATH` | SQLite `data/erlbrew.db` |
| `FAQ_PATH` / `KNOWLEDGE_PATH` | live-editable files (`data/…`) |
| `PRIVACY_PATH` | optional privacy HTML (`data/privacy.html`) |
| `POS_MYSQL_URL` | read-only POS MySQL URL (empty = no live prices) |
| `POS_MENU_QUERY` | optional custom menu query |
| `MENU_CACHE_TTL_SEC` | POS menu cache TTL (60) |
| `RATE_LIMIT_MAX` / `RATE_LIMIT_WINDOW_SEC` | 15 / 600 |
| `MAX_TEXT_CHARS` | per-message input cap (500) |
| `PORT` / `LOG_LEVEL` | server basics |

## Editable content (no restart / no rebuild)

- **`faq.yaml`** — static Taglish answers for menu, hours+location, promo,
  wifi, delivery, pickup, parking, events + the welcome message. Replace all
  `[PLACEHOLDER: …]` blocks. Entries support `keywords` (whole words),
  `patterns` (substrings), `regex` (careful) and optional `quick_replies`.
- **`knowledge.md`** — the LLM's system prompt (≤ ~400 words, English — keep
  it tight so the tiny model stays on track).
- Both files are re-read the moment their mtime changes; wrong YAML only logs
  an error (last good copy keeps being used).
- Event/order form wording and canned texts live in `src/events.rs`,
  `src/orders.rs` and `src/config.rs` and need a rebuild.

## Ops cheat-sheet

```bash
# pause state lives in SQLite; if a customer had a bad handoff later:
curl -X POST https://bot.example.com/admin/unpause/PSID_123 \
     -H "X-Admin-Token: $ADMIN_TOKEN"

# health
curl https://bot.example.com/health

# leads so far (they are also Telegram-alerted):
sqlite3 data/erlbrew.db 'SELECT * FROM leads;'

# newest orders (token-protected):
curl https://bot.example.com/admin/orders?limit=10 \
     -H "X-Admin-Token: $ADMIN_TOKEN"
```

## Limits & notes

- Uses Messenger's **standard messaging window** only: the bot replies within
  24 h of receiving a customer message. No message tags, no broadcasts, no
  re-marketing blasts.
- Never says "yes, your event is confirmed": even the final form message
  explicitly states the team will confirm availability & price.
- Logs are structured JSON but contain **no tokens or secrets**; Graph auth
  uses a Bearer header so the token never appears in a URL either.
