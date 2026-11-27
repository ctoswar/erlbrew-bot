//! SQLite storage (WAL) — all bot state.
//!
//! The schema is byte-for-byte compatible with the original Python bot so an
//! existing `data/erlbrew.db` can be reused without a migration step.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::config::Settings;

const SCHEMA: &str = r#"
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
CREATE TABLE IF NOT EXISTS bot_messages (
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
"#;

/// Abandoned forms self-destruct after 2 hours.
pub const FORM_TTL_SEC: i64 = 2 * 60 * 60;

#[derive(Clone, Debug)]
pub struct FormRow {
    pub step: String,
    pub data: HashMap<String, String>,
    pub tries: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct OrderRow {
    pub id: i64,
    pub ts: i64,
    pub psid: String,
    pub items: String,
    pub name: String,
    pub address: String,
    pub contact: String,
    pub menu_note: String,
    pub total_estimate: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Db {
    /// Open (and create) the database. The parent directory is created.
    pub fn connect(path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        tracing::info!(path, "db connected");
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a blocking SQLite closure on the blocking pool.
    async fn run<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().expect("db mutex poisoned");
            f(&mut guard)
        })
        .await?;
        Ok(result?)
    }

    // --- users -----------------------------------------------------------
    pub async fn upsert_user(&self, psid: &str) -> anyhow::Result<()> {
        let psid = psid.to_string();
        self.run(move |c| {
            let now = now();
            c.execute(
                "INSERT INTO users (psid, name, paused, created_at, updated_at) \
                 VALUES (?1, '', 0, ?2, ?2) \
                 ON CONFLICT(psid) DO UPDATE SET updated_at=excluded.updated_at",
                params![psid, now],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_user_name(&self, psid: &str) -> anyhow::Result<String> {
        let psid = psid.to_string();
        self.run(move |c| {
            let name: Option<String> = c
                .query_row("SELECT name FROM users WHERE psid=?1", params![psid], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(name.unwrap_or_default())
        })
        .await
    }

    pub async fn set_user_name(&self, psid: &str, name: &str) -> anyhow::Result<()> {
        let psid = psid.to_string();
        let name = name.to_string();
        self.run(move |c| {
            c.execute(
                "UPDATE users SET name=?1, updated_at=?2 WHERE psid=?3",
                params![name, now(), psid],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_paused(&self, psid: &str) -> anyhow::Result<bool> {
        let psid = psid.to_string();
        self.run(move |c| {
            let paused: Option<i64> = c
                .query_row(
                    "SELECT paused FROM users WHERE psid=?1",
                    params![psid],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(paused.unwrap_or(0) != 0)
        })
        .await
    }

    /// Set the pause flag. Returns false when the user row does not exist.
    pub async fn set_paused(&self, psid: &str, paused: bool) -> anyhow::Result<bool> {
        let psid = psid.to_string();
        self.run(move |c| {
            let changed = c.execute(
                "UPDATE users SET paused=?1, updated_at=?2 WHERE psid=?3",
                params![i64::from(paused), now(), psid],
            )?;
            Ok(changed > 0)
        })
        .await
    }

    /// CLI/admin helper: create-or-freshen the row and clear paused.
    pub async fn unpause_user(&self, psid: &str) -> anyhow::Result<bool> {
        self.upsert_user(psid).await?;
        self.set_paused(psid, false).await
    }

    // --- dedupe + rate limit + notices -----------------------------------
    /// Insert-or-ignore a message id. True when it is new.
    pub async fn mark_message_seen(&self, mid: &str) -> anyhow::Result<bool> {
        let mid = mid.to_string();
        self.run(move |c| {
            let changed = c.execute(
                "INSERT OR IGNORE INTO seen_messages (mid, created_at) VALUES (?1, ?2)",
                params![mid, now()],
            )?;
            Ok(changed > 0)
        })
        .await
    }

    pub async fn mark_bot_message(&self, mid: &str) -> anyhow::Result<()> {
        let mid = mid.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO bot_messages (mid, created_at) VALUES (?1, ?2)",
                params![mid, now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn is_bot_message(&self, mid: &str) -> anyhow::Result<bool> {
        if mid.is_empty() {
            return Ok(false);
        }
        let mid = mid.to_string();
        self.run(move |c| {
            let found: Option<i64> = c
                .query_row("SELECT 1 FROM bot_messages WHERE mid=?1", params![mid], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(found.is_some())
        })
        .await
    }

    pub async fn note_rate_event(&self, psid: &str) -> anyhow::Result<()> {
        let psid = psid.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT INTO rate_events (psid, ts) VALUES (?1, ?2)",
                params![psid, now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn count_rate_events(&self, psid: &str, window_sec: i64) -> anyhow::Result<i64> {
        let psid = psid.to_string();
        self.run(move |c| {
            let count: i64 = c.query_row(
                "SELECT COUNT(*) FROM rate_events WHERE psid=?1 AND ts > ?2",
                params![psid, now() - window_sec],
                |r| r.get(0),
            )?;
            Ok(count)
        })
        .await
    }

    /// True only the first time `kind` is raised inside the current window.
    pub async fn pop_notice_once(
        &self,
        psid: &str,
        kind: &str,
        window_sec: i64,
    ) -> anyhow::Result<bool> {
        let psid = psid.to_string();
        let kind = kind.to_string();
        self.run(move |c| {
            let now = now();
            let previous: Option<i64> = c
                .query_row(
                    "SELECT ts FROM notices WHERE psid=?1 AND kind=?2",
                    params![psid, kind],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(ts) = previous {
                if now - ts < window_sec {
                    return Ok(false);
                }
            }
            c.execute(
                "INSERT INTO notices (psid, kind, ts) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(psid, kind) DO UPDATE SET ts=excluded.ts",
                params![psid, kind, now],
            )?;
            Ok(true)
        })
        .await
    }

    // --- forms -----------------------------------------------------------
    /// Active form state; stale forms are dropped automatically.
    pub async fn get_form(&self, psid: &str) -> anyhow::Result<Option<FormRow>> {
        let psid = psid.to_string();
        self.run(move |c| {
            let row: Option<(String, String, i64, i64)> = c
                .query_row(
                    "SELECT step, data, tries, updated_at FROM event_forms WHERE psid=?1",
                    params![psid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((step, data, tries, updated_at)) = row else {
                return Ok(None);
            };
            if now() - updated_at > FORM_TTL_SEC {
                c.execute("DELETE FROM event_forms WHERE psid=?1", params![psid])?;
                tracing::info!(psid, step, "stale form dropped");
                return Ok(None);
            }
            let data = serde_json::from_str::<HashMap<String, String>>(&data).unwrap_or_default();
            Ok(Some(FormRow { step, data, tries }))
        })
        .await
    }

    pub async fn set_form(
        &self,
        psid: &str,
        step: &str,
        data: &HashMap<String, String>,
        tries: i64,
    ) -> anyhow::Result<()> {
        let psid = psid.to_string();
        let step = step.to_string();
        let data = serde_json::to_string(data)?;
        self.run(move |c| {
            c.execute(
                "INSERT INTO event_forms (psid, step, data, tries, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(psid) DO UPDATE SET step=excluded.step, data=excluded.data, \
                 tries=excluded.tries, updated_at=excluded.updated_at",
                params![psid, step, data, tries, now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn delete_form(&self, psid: &str) -> anyhow::Result<()> {
        let psid = psid.to_string();
        self.run(move |c| {
            c.execute("DELETE FROM event_forms WHERE psid=?1", params![psid])?;
            Ok(())
        })
        .await
    }

    // --- event leads -----------------------------------------------------
    pub async fn add_lead(
        &self,
        psid: &str,
        event_type: &str,
        event_when: &str,
        guests: i64,
        name: &str,
        contact: &str,
    ) -> anyhow::Result<i64> {
        let psid = psid.to_string();
        let event_type = event_type.to_string();
        let event_when = event_when.to_string();
        let name = name.to_string();
        let contact = contact.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT INTO event_leads (psid, event_type, event_when, guests, name, contact, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![psid, event_type, event_when, guests, name, contact, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    // --- orders ----------------------------------------------------------
    #[allow(clippy::too_many_arguments)]
    pub async fn add_order(
        &self,
        psid: &str,
        items: &str,
        name: &str,
        address: &str,
        contact: &str,
        menu_note: &str,
        total_estimate: &str,
    ) -> anyhow::Result<i64> {
        let psid = psid.to_string();
        let items = items.to_string();
        let name = name.to_string();
        let address = address.to_string();
        let contact = contact.to_string();
        let menu_note = menu_note.to_string();
        let total_estimate = total_estimate.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT INTO orders (ts, psid, items, name, address, contact, menu_note, \
                 total_estimate, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'new')",
                params![
                    now(),
                    psid,
                    items,
                    name,
                    address,
                    contact,
                    menu_note,
                    total_estimate
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    /// Newest orders first (admin view).
    pub async fn list_orders(&self, limit: i64) -> anyhow::Result<Vec<OrderRow>> {
        self.run(move |c| {
            let mut stmt = c.prepare(
                "SELECT id, ts, psid, items, name, address, contact, menu_note, \
                 total_estimate, status FROM orders ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |r| {
                Ok(OrderRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    psid: r.get(2)?,
                    items: r.get(3)?,
                    name: r.get(4)?,
                    address: r.get(5)?,
                    contact: r.get(6)?,
                    menu_note: r.get(7)?,
                    total_estimate: r.get(8)?,
                    status: r.get(9)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    }

    // --- chat history ----------------------------------------------------
    pub async fn clear_history(&self, psid: &str) -> anyhow::Result<()> {
        let psid = psid.to_string();
        self.run(move |c| {
            c.execute("DELETE FROM chat_history WHERE psid=?1", params![psid])?;
            Ok(())
        })
        .await
    }

    pub async fn append_history(&self, psid: &str, role: &str, content: &str) -> anyhow::Result<()> {
        let psid = psid.to_string();
        let role = role.to_string();
        let content: String = content.chars().take(2000).collect();
        self.run(move |c| {
            c.execute(
                "INSERT INTO chat_history (psid, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![psid, role, content, now()],
            )?;
            Ok(())
        })
        .await
    }

    /// Last `limit` messages (oldest first) within the inactivity TTL.
    pub async fn get_history(
        &self,
        psid: &str,
        limit: i64,
        ttl_sec: i64,
    ) -> anyhow::Result<Vec<HistoryMessage>> {
        let psid = psid.to_string();
        self.run(move |c| {
            let mut stmt = c.prepare(
                "SELECT role, content FROM chat_history \
                 WHERE psid=?1 AND created_at > ?2 ORDER BY created_at DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![psid, now() - ttl_sec, limit.max(1)], |r| {
                Ok(HistoryMessage {
                    role: r.get(0)?,
                    content: r.get(1)?,
                })
            })?;
            let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            out.reverse();
            Ok(out)
        })
        .await
    }

    // --- cleanup ---------------------------------------------------------
    pub async fn cleanup(
        &self,
        seen_ttl_sec: i64,
        history_ttl_sec: i64,
        form_ttl_sec: i64,
    ) -> anyhow::Result<()> {
        self.run(move |c| {
            let now = now();
            c.execute(
                "DELETE FROM seen_messages WHERE created_at < ?1",
                params![now - seen_ttl_sec],
            )?;
            c.execute(
                "DELETE FROM bot_messages WHERE created_at < ?1",
                params![now - seen_ttl_sec],
            )?;
            c.execute(
                "DELETE FROM chat_history WHERE created_at < ?1",
                params![now - history_ttl_sec],
            )?;
            c.execute(
                "DELETE FROM event_forms WHERE updated_at < ?1",
                params![now - form_ttl_sec],
            )?;
            Ok(())
        })
        .await
    }
}

impl Db {
    pub async fn cleanup_from_settings(&self, settings: &Settings) -> anyhow::Result<()> {
        self.cleanup(24 * 3600, settings.llm_history_ttl_sec, settings.event_form_ttl_sec)
            .await
    }
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut cols: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(orders)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for row in rows {
            cols.push(row?);
        }
    }
    if !cols.iter().any(|c| c == "menu_note") {
        conn.execute(
            "ALTER TABLE orders ADD COLUMN menu_note TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !cols.iter().any(|c| c == "total_estimate") {
        conn.execute(
            "ALTER TABLE orders ADD COLUMN total_estimate TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    Ok(())
}
