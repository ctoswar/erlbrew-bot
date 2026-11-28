//! SQLite persistence: users/pauses, dedupe, rate limiting, form state, leads, orders.
//!
//! Queries are tiny, so a single connection behind a std `Mutex` is plenty. The lock is
//! never held across an `.await`.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current step of a customer's active form.
#[derive(Clone, Debug)]
pub struct FormRow {
    pub step: String,
    pub data: HashMap<String, String>,
    pub tries: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    psid TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    paused INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS seen_messages (
    mid TEXT PRIMARY KEY,
    seen_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rate_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    psid TEXT NOT NULL,
    ts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rate_events_psid_ts ON rate_events (psid, ts);
CREATE TABLE IF NOT EXISTS notices (
    psid TEXT NOT NULL,
    kind TEXT NOT NULL,
    last_sent INTEGER NOT NULL,
    PRIMARY KEY (psid, kind)
);
CREATE TABLE IF NOT EXISTS forms (
    psid TEXT PRIMARY KEY,
    step TEXT NOT NULL,
    data TEXT NOT NULL DEFAULT '{}',
    tries INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS leads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    psid TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_when TEXT NOT NULL,
    guests INTEGER NOT NULL,
    name TEXT NOT NULL,
    contact TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    psid TEXT NOT NULL,
    items TEXT NOT NULL,
    name TEXT NOT NULL,
    address TEXT NOT NULL,
    contact TEXT NOT NULL,
    total_estimate TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
";

impl Db {
    pub fn connect(path: &str) -> Result<Self> {
        if path != ":memory:" {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating db directory {}", parent.display()))?;
                }
            }
        }
        let conn = Connection::open(path).with_context(|| format!("opening sqlite db {path}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_mode returns a row, so go through query_row (ignored for :memory:).
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Cheap liveness probe for /health.
    pub fn connected(&self) -> bool {
        self.lock().query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok()
    }

    // ------------------------------------------------------------------ users

    pub fn upsert_user(&self, psid: &str) -> Result<()> {
        let t = now();
        self.lock().execute(
            "INSERT INTO users (psid, created_at, updated_at) VALUES (?1, ?2, ?2)
             ON CONFLICT(psid) DO UPDATE SET updated_at = excluded.updated_at",
            params![psid, t],
        )?;
        Ok(())
    }

    pub fn get_paused(&self, psid: &str) -> Result<bool> {
        let v: Option<i64> = self
            .lock()
            .query_row("SELECT paused FROM users WHERE psid = ?1", [psid], |r| r.get(0))
            .optional()?;
        Ok(v.unwrap_or(0) != 0)
    }

    pub fn set_paused(&self, psid: &str, paused: bool) -> Result<()> {
        self.upsert_user(psid)?;
        self.lock().execute(
            "UPDATE users SET paused = ?2, updated_at = ?3 WHERE psid = ?1",
            params![psid, paused as i64, now()],
        )?;
        Ok(())
    }

    /// Returns true when a paused user was actually found and unpaused.
    pub fn unpause_user(&self, psid: &str) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE users SET paused = 0, updated_at = ?2 WHERE psid = ?1",
            params![psid, now()],
        )?;
        Ok(n > 0)
    }

    pub fn get_user_name(&self, psid: &str) -> Result<String> {
        let v: Option<String> = self
            .lock()
            .query_row("SELECT name FROM users WHERE psid = ?1", [psid], |r| r.get(0))
            .optional()?;
        Ok(v.unwrap_or_default())
    }

    pub fn set_user_name(&self, psid: &str, name: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE users SET name = ?2, updated_at = ?3 WHERE psid = ?1",
            params![psid, name, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------- dedupe / rate

    /// Records `mid`; returns true the FIRST time it is seen, false for a duplicate delivery.
    pub fn mark_message_seen(&self, mid: &str) -> Result<bool> {
        let n = self.lock().execute(
            "INSERT OR IGNORE INTO seen_messages (mid, seen_at) VALUES (?1, ?2)",
            params![mid, now()],
        )?;
        Ok(n == 1)
    }

    pub fn note_rate_event(&self, psid: &str) -> Result<()> {
        self.lock()
            .execute("INSERT INTO rate_events (psid, ts) VALUES (?1, ?2)", params![psid, now()])?;
        Ok(())
    }

    pub fn count_rate_events(&self, psid: &str, window_sec: i64) -> Result<i64> {
        Ok(self.lock().query_row(
            "SELECT COUNT(*) FROM rate_events WHERE psid = ?1 AND ts >= ?2",
            params![psid, now() - window_sec],
            |r| r.get(0),
        )?)
    }

    /// True when a one-off notice of `kind` may be sent now (none sent within `cooldown_sec`).
    /// Records the send, so the next call inside the cooldown returns false.
    pub fn pop_notice_once(&self, psid: &str, kind: &str, cooldown_sec: i64) -> Result<bool> {
        let t = now();
        let conn = self.lock();
        let last: Option<i64> = conn
            .query_row(
                "SELECT last_sent FROM notices WHERE psid = ?1 AND kind = ?2",
                params![psid, kind],
                |r| r.get(0),
            )
            .optional()?;
        if last.is_some_and(|l| t - l < cooldown_sec) {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO notices (psid, kind, last_sent) VALUES (?1, ?2, ?3)
             ON CONFLICT(psid, kind) DO UPDATE SET last_sent = excluded.last_sent",
            params![psid, kind, t],
        )?;
        Ok(true)
    }

    /// Drop old bookkeeping rows (called hourly from main).
    pub fn prune(&self) -> Result<()> {
        let t = now();
        let conn = self.lock();
        conn.execute("DELETE FROM seen_messages WHERE seen_at < ?1", [t - 7 * 86_400])?;
        conn.execute("DELETE FROM rate_events WHERE ts < ?1", [t - 86_400])?;
        conn.execute("DELETE FROM forms WHERE updated_at < ?1", [t - 7 * 86_400])?;
        Ok(())
    }

    // ------------------------------------------------------------------ forms

    pub fn get_form(&self, psid: &str) -> Result<Option<FormRow>> {
        let row: Option<(String, String, i64)> = self
            .lock()
            .query_row(
                "SELECT step, data, tries FROM forms WHERE psid = ?1",
                [psid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row.map(|(step, data, tries)| FormRow {
            step,
            data: serde_json::from_str(&data).unwrap_or_default(),
            tries,
        }))
    }

    pub fn set_form(
        &self,
        psid: &str,
        step: &str,
        data: &HashMap<String, String>,
        tries: i64,
    ) -> Result<()> {
        let data = serde_json::to_string(data)?;
        self.lock().execute(
            "INSERT INTO forms (psid, step, data, tries, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(psid) DO UPDATE SET
                step = excluded.step, data = excluded.data,
                tries = excluded.tries, updated_at = excluded.updated_at",
            params![psid, step, data, tries, now()],
        )?;
        Ok(())
    }

    pub fn delete_form(&self, psid: &str) -> Result<()> {
        self.lock().execute("DELETE FROM forms WHERE psid = ?1", [psid])?;
        Ok(())
    }

    // -------------------------------------------------------- leads & orders

    pub fn add_lead(
        &self,
        psid: &str,
        event_type: &str,
        event_when: &str,
        guests: i64,
        name: &str,
        contact: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO leads (psid, event_type, event_when, guests, name, contact, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![psid, event_type, event_when, guests, name, contact, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn add_order(
        &self,
        psid: &str,
        items: &str,
        name: &str,
        address: &str,
        contact: &str,
        total_estimate: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO orders (psid, items, name, address, contact, total_estimate, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![psid, items, name, address, contact, total_estimate, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_orders(&self, limit: i64) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, psid, items, name, address, contact, total_estimate, created_at
             FROM orders ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "psid": r.get::<_, String>(1)?,
                "items": r.get::<_, String>(2)?,
                "name": r.get::<_, String>(3)?,
                "address": r.get::<_, String>(4)?,
                "contact": r.get::<_, String>(5)?,
                "total_estimate": r.get::<_, String>(6)?,
                "created_at": r.get::<_, i64>(7)?,
            }))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::connect(":memory:").unwrap()
    }

    #[test]
    fn dedupe_and_pause() {
        let db = db();
        assert!(db.mark_message_seen("m1").unwrap());
        assert!(!db.mark_message_seen("m1").unwrap());

        assert!(!db.get_paused("u1").unwrap());
        db.set_paused("u1", true).unwrap();
        assert!(db.get_paused("u1").unwrap());
        assert!(db.unpause_user("u1").unwrap());
        assert!(!db.get_paused("u1").unwrap());
        assert!(!db.unpause_user("nobody").unwrap());
    }

    #[test]
    fn notices_and_rate() {
        let db = db();
        assert!(db.pop_notice_once("u", "rate", 600).unwrap());
        assert!(!db.pop_notice_once("u", "rate", 600).unwrap());
        for _ in 0..3 {
            db.note_rate_event("u").unwrap();
        }
        assert_eq!(db.count_rate_events("u", 600).unwrap(), 3);
    }

    #[test]
    fn forms_and_orders() {
        let db = db();
        let mut data = HashMap::new();
        data.insert("items".to_string(), "2 latte".to_string());
        db.set_form("u", "order_name", &data, 1).unwrap();
        let row = db.get_form("u").unwrap().unwrap();
        assert_eq!((row.step.as_str(), row.tries), ("order_name", 1));
        assert_eq!(row.data["items"], "2 latte");
        db.delete_form("u").unwrap();
        assert!(db.get_form("u").unwrap().is_none());

        let id = db.add_order("u", "2 latte", "Ana", "Kawit", "09171234567", "258").unwrap();
        assert_eq!(db.list_orders(10).unwrap()[0]["id"], id);
        assert!(db.add_lead("u", "birthday", "May 12", 10, "Ana", "09171234567").unwrap() > 0);
    }
}
