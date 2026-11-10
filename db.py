"""SQLite storage (aiosqlite, async, WAL) — all bot state.

Tables:
  users          PSID -> profile name, paused flag
  seen_messages  webhook message-id dedupe
  rate_events    per-PSID inbound message timestamps (rate limiting)
  notices        one-time polite notices (rate limit / too long)
  event_forms    active event booking form state (per PSID)
  event_leads    completed event bookings
  chat_history   last messages per user for the LLM fallback
"""
from __future__ import annotations

import json
import logging
import os
import time
from typing import Any, Dict, List, Optional, Tuple

import aiosqlite

log = logging.getLogger("db")

_SCHEMA = """
CREATE TABLE IF NOT EXISTS users (
    psid        TEXT PRIMARY KEY,
    name        TEXT NOT NULL DEFAULT '',
    paused      INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS seen_messages (
    mid         TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rate_events (
    psid        TEXT NOT NULL,
    ts          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rate_psid_ts ON rate_events (psid, ts);
CREATE TABLE IF NOT EXISTS notices (
    psid        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    PRIMARY KEY (psid, kind)
);
CREATE TABLE IF NOT EXISTS event_forms (
    psid        TEXT PRIMARY KEY,
    step        TEXT NOT NULL,
    data        TEXT NOT NULL,
    tries       INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS event_leads (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    psid        TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    event_when  TEXT NOT NULL,
    guests      INTEGER NOT NULL,
    name        TEXT NOT NULL,
    contact     TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'new',
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS orders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    psid        TEXT NOT NULL,
    items       TEXT NOT NULL,
    name        TEXT NOT NULL,
    address     TEXT NOT NULL,
    contact     TEXT NOT NULL,
    menu_note   TEXT NOT NULL DEFAULT '',
    total_estimate TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'new'
);
CREATE TABLE IF NOT EXISTS chat_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    psid        TEXT NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_history_psid ON chat_history (psid, created_at);
"""

_conn: Optional[aiosqlite.Connection] = None


def _now() -> int:
    return int(time.time())


async def connect(path: str) -> None:
    """Open (and create) the database. db_path's parent directory is created."""
    global _conn
    if _conn is not None:
        return
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    conn = await aiosqlite.connect(path)
    conn.row_factory = aiosqlite.Row
    await conn.execute("PRAGMA journal_mode=WAL")
    await conn.execute("PRAGMA synchronous=NORMAL")
    await conn.executescript(_SCHEMA)
    await _migrate(conn)
    await conn.commit()
    _conn = conn
    log.info("db connected", extra={"path": path})


async def _migrate(conn: aiosqlite.Connection) -> None:
    """Add columns that pre-existing databases are missing (idempotent)."""
    try:
        cur = await conn.execute("PRAGMA table_info(orders)")
        cols = {row[1] for row in await cur.fetchall()}
    except Exception:
        return
    if "menu_note" not in cols:
        await conn.execute("ALTER TABLE orders ADD COLUMN menu_note TEXT NOT NULL DEFAULT ''")
    if "total_estimate" not in cols:
        await conn.execute(
            "ALTER TABLE orders ADD COLUMN total_estimate TEXT NOT NULL DEFAULT ''"
        )


async def close() -> None:
    global _conn
    if _conn is None:
        return
    await _conn.close()
    _conn = None


def connected() -> bool:
    return _conn is not None


async def _fetchone(sql: str, params: Tuple[Any, ...] = ()) -> Optional[Any]:
    assert _conn is not None, "db.connect() not called"
    cur = await _conn.execute(sql, params)
    return await cur.fetchone()


# --- users -------------------------------------------------------------------
async def upsert_user(psid: str) -> None:
    assert _conn is not None
    now = _now()
    await _conn.execute(
        "INSERT INTO users (psid, name, paused, created_at, updated_at) VALUES (?, '', 0, ?, ?)"
        " ON CONFLICT(psid) DO UPDATE SET updated_at=excluded.updated_at",
        (psid, now, now),
    )
    await _conn.commit()


async def get_user_name(psid: str) -> str:
    row = await _fetchone("SELECT name FROM users WHERE psid=?", (psid,))
    return (row["name"] if row else "") or ""


async def set_user_name(psid: str, name: str) -> None:
    assert _conn is not None
    await _conn.execute(
        "UPDATE users SET name=?, updated_at=? WHERE psid=?", (name, _now(), psid)
    )
    await _conn.commit()


async def get_paused(psid: str) -> bool:
    row = await _fetchone("SELECT paused FROM users WHERE psid=?", (psid,))
    return bool(row and row["paused"])


async def set_paused(psid: str, paused: bool) -> bool:
    """Set the pause flag. Returns False if the user row does not exist."""
    assert _conn is not None
    cur = await _conn.execute(
        "UPDATE users SET paused=?, updated_at=? WHERE psid=?",
        (1 if paused else 0, _now(), psid),
    )
    await _conn.commit()
    return cur.rowcount > 0


async def unpause_user(psid: str) -> bool:
    """CLI/admin helper: create-or-freshen the row and clear paused."""
    await upsert_user(psid)
    return await set_paused(psid, False)


# --- dedupe + rate limit + notices --------------------------------------------
async def mark_message_seen(mid: str) -> bool:
    """Insert-or-ignore a message id. True if it is new (should process)."""
    assert _conn is not None
    cur = await _conn.execute(
        "INSERT OR IGNORE INTO seen_messages (mid, created_at) VALUES (?, ?)",
        (mid, _now()),
    )
    await _conn.commit()
    return cur.rowcount > 0


async def note_rate_event(psid: str) -> None:
    assert _conn is not None
    await _conn.execute(
        "INSERT INTO rate_events (psid, ts) VALUES (?, ?)", (psid, _now())
    )
    await _conn.commit()


async def count_rate_events(psid: str, window_sec: int) -> int:
    row = await _fetchone(
        "SELECT COUNT(*) AS n FROM rate_events WHERE psid=? AND ts > ?",
        (psid, _now() - window_sec),
    )
    return int(row["n"]) if row else 0


async def pop_notice_once(psid: str, kind: str, window_sec: int) -> bool:
    """True only the first time `kind` is raised inside the current window."""
    assert _conn is not None
    now = _now()
    row = await _fetchone(
        "SELECT ts FROM notices WHERE psid=? AND kind=?", (psid, kind)
    )
    if row and now - int(row["ts"]) < window_sec:
        return False
    await _conn.execute(
        "INSERT INTO notices (psid, kind, ts) VALUES (?, ?, ?)"
        " ON CONFLICT(psid, kind) DO UPDATE SET ts=excluded.ts",
        (psid, kind, now),
    )
    await _conn.commit()
    return True


# --- event forms --------------------------------------------------------------
FORM_TTL_SEC = 2 * 60 * 60  # stale abandoned forms self-destruct after 2 h


async def get_form(psid: str) -> Optional[Tuple[str, Dict[str, Any], int]]:
    """Returns (step, data, tries) or None. Stale forms (older than TTL) are
    dropped automatically so leftovers can never hijack a new conversation."""
    row = await _fetchone("SELECT step, data, tries, updated_at FROM event_forms WHERE psid=?", (psid,))
    if row is None:
        return None
    if _now() - int(row["updated_at"]) > FORM_TTL_SEC:
        await delete_form(psid)
        log.info("stale form dropped", extra={"psid": psid, "step": row["step"]})
        return None
    try:
        data = json.loads(row["data"])
    except (TypeError, ValueError):
        data = {}
    if not isinstance(data, dict):
        data = {}
    return str(row["step"]), data, int(row["tries"])


async def set_form(psid: str, step: str, data: Dict[str, Any], tries: int) -> None:
    assert _conn is not None
    await _conn.execute(
        "INSERT INTO event_forms (psid, step, data, tries, updated_at) VALUES (?, ?, ?, ?, ?)"
        " ON CONFLICT(psid) DO UPDATE SET step=excluded.step, data=excluded.data,"
        " tries=excluded.tries, updated_at=excluded.updated_at",
        (psid, step, json.dumps(data, ensure_ascii=False), tries, _now()),
    )
    await _conn.commit()


async def delete_form(psid: str) -> None:
    assert _conn is not None
    await _conn.execute("DELETE FROM event_forms WHERE psid=?", (psid,))
    await _conn.commit()


# --- event leads ---------------------------------------------------------------
async def add_lead(
    psid: str, event_type: str, event_when: str, guests: int, name: str, contact: str
) -> int:
    assert _conn is not None
    cur = await _conn.execute(
        "INSERT INTO event_leads (psid, event_type, event_when, guests, name, contact, created_at)"
        " VALUES (?, ?, ?, ?, ?, ?, ?)",
        (psid, event_type, event_when, guests, name, contact, _now()),
    )
    await _conn.commit()
    return int(cur.lastrowid or 0)


# --- orders -------------------------------------------------------------------
async def add_order(
    psid: str,
    items: str,
    name: str,
    address: str,
    contact: str,
    menu_note: str = "",
    total_estimate: str = "",
) -> int:
    assert _conn is not None
    cur = await _conn.execute(
        "INSERT INTO orders (ts, psid, items, name, address, contact, menu_note,"
        " total_estimate, status)"
        " VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'new')",
        (_now(), psid, items, name, address, contact, menu_note, total_estimate),
    )
    await _conn.commit()
    return int(cur.lastrowid or 0)


async def list_orders(limit: int = 50) -> List[Dict[str, Any]]:
    """Newest orders first (admin view)."""
    assert _conn is not None
    cur = await _conn.execute(
        "SELECT id, ts, psid, items, name, address, contact, menu_note,"
        " total_estimate, status"
        " FROM orders ORDER BY id DESC LIMIT ?",
        (limit,),
    )
    rows = await cur.fetchall()
    return [dict(row) for row in rows]


# --- chat history ---------------------------------------------------------------
async def clear_history(psid: str) -> None:
    assert _conn is not None
    await _conn.execute("DELETE FROM chat_history WHERE psid=?", (psid,))
    await _conn.commit()


async def append_history(psid: str, role: str, content: str) -> None:
    assert _conn is not None
    await _conn.execute(
        "INSERT INTO chat_history (psid, role, content, created_at) VALUES (?, ?, ?, ?)",
        (psid, role, content[:2000], _now()),
    )
    await _conn.commit()


async def get_history(psid: str, limit: int, ttl_sec: int) -> List[Dict[str, str]]:
    """Last `limit` messages (oldest first) within the inactivity TTL."""
    rows = await _fetchone_all(
        "SELECT role, content FROM chat_history"
        " WHERE psid=? AND created_at > ? ORDER BY created_at DESC LIMIT ?",
        (psid, _now() - ttl_sec, max(1, limit)),
    )
    return [{"role": r["role"], "content": r["content"]} for r in reversed(rows)]


async def _fetchone_all(sql: str, params: Tuple[Any, ...] = ()) -> List[Any]:
    assert _conn is not None
    cur = await _conn.execute(sql, params)
    return list(await cur.fetchall())


# --- cleanup ----------------------------------------------------------------------
async def cleanup(
    *, seen_ttl_sec: int = 86400, history_ttl_sec: int = 86400, form_ttl_sec: int = 6 * 3600
) -> None:
    """Delete stale dedupe rows, rate events, chat history and dead forms."""
    assert _conn is not None
    now = _now()
    await _conn.execute("DELETE FROM seen_messages WHERE created_at < ?", (now - seen_ttl_sec,))
    await _conn.execute("DELETE FROM chat_history WHERE created_at < ?", (now - history_ttl_sec,))
    await _conn.execute("DELETE FROM event_forms WHERE updated_at < ?", (now - form_ttl_sec,))
    await _conn.commit()
