//! Durable SQLite inbox: inbound Slack messages survive a daemon restart.
//! `queued` -> `running` -> `done`/`failed`; `recover` re-queues `running`.
//! Slack event keys (channel + ts) dedupe socket-mode redeliveries.
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inbox (
    id TEXT PRIMARY KEY, ext_id TEXT UNIQUE, channel TEXT NOT NULL,
    thread_ts TEXT NOT NULL DEFAULT '', sender TEXT NOT NULL,
    conversation TEXT NOT NULL,
    text TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'queued',
    created REAL NOT NULL, reply TEXT, error TEXT);
CREATE TABLE IF NOT EXISTS pairing (
    code TEXT PRIMARY KEY, user_id TEXT NOT NULL, created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
";

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub channel: String,
    /// Root ts of the Slack thread the reply belongs in ("" = top level).
    pub thread_ts: String,
    pub sender: String,
    pub conversation: String,
    pub text: String,
    pub state: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

pub fn uuid_hex() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    } else {
        // Fallback: time + pid entropy — ids only need uniqueness.
        let nanos = now_secs().to_bits();
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn map_err(e: rusqlite::Error) -> String {
    format!("store error: {e}")
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        channel: r.get(1)?,
        thread_ts: r.get(2)?,
        sender: r.get(3)?,
        conversation: r.get(4)?,
        text: r.get(5)?,
        state: r.get(6)?,
    })
}

impl Store {
    pub fn new(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "cannot create state dir".to_string())?;
        }
        let conn = Connection::open(path).map_err(map_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Default location beside the config file.
    pub fn default_path(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("queue.sqlite")
    }

    /// Accept one inbound message. `ext_id` is Slack's channel+ts key;
    /// a socket-mode redelivery inserts nothing and returns `None`.
    /// `Err` when the queue is at capacity.
    pub fn enqueue(
        &self,
        ext_id: &str,
        channel: &str,
        thread_ts: &str,
        sender: &str,
        conversation: &str,
        text: &str,
        capacity: u64,
    ) -> Result<Option<String>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM inbox WHERE state='queued'", [], |r| {
                r.get(0)
            })
            .map_err(map_err)?;
        if pending >= capacity.max(1) as i64 {
            return Err("queue is full".to_string());
        }
        let id = uuid_hex();
        let n = conn
            .execute(
                "INSERT OR IGNORE INTO inbox \
                 (id,ext_id,channel,thread_ts,sender,conversation,text,state,created) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'queued',?8)",
                rusqlite::params![
                    id,
                    ext_id,
                    channel,
                    thread_ts,
                    sender,
                    conversation,
                    text,
                    now_secs()
                ],
            )
            .map_err(map_err)?;
        Ok((n > 0).then_some(id))
    }

    /// Oldest queued item, atomically marked `running`.
    pub fn claim(&self) -> Result<Option<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let row = conn
            .query_row(
                "SELECT id,channel,thread_ts,sender,conversation,text,state \
                 FROM inbox WHERE state='queued' ORDER BY created LIMIT 1",
                [],
                row_to_item,
            )
            .ok();
        match row {
            Some(item) => {
                conn.execute(
                    "UPDATE inbox SET state='running' WHERE id=?1",
                    rusqlite::params![item.id],
                )
                .map_err(map_err)?;
                Ok(Some(Item {
                    state: "running".to_string(),
                    ..item
                }))
            }
            None => Ok(None),
        }
    }

    /// Put a claimed item back (a Busy conversation holds its lock).
    pub fn requeue(&self, id: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='queued' WHERE id=?1 AND state='running'",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn complete(&self, id: &str, reply: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='done', reply=?2 WHERE id=?1",
                rusqlite::params![id, reply],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn fail(&self, id: &str, error: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='failed', error=?2 WHERE id=?1",
                rusqlite::params![id, error],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// List queued/running items for `queue list`.
    pub fn items(&self) -> Result<Vec<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id,channel,thread_ts,sender,conversation,text,state \
                 FROM inbox WHERE state IN ('queued','running') ORDER BY created",
            )
            .map_err(map_err)?;
        let rows = stmt.query_map([], row_to_item).map_err(map_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_err)
    }

    /// Cancel a still-queued item.
    pub fn cancel(&self, id: &str) -> Result<bool, String> {
        let n = self
            .conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='cancelled' WHERE id=?1 AND state='queued'",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(n > 0)
    }

    /// Restart recovery: `running` rows were interrupted — re-queue them.
    /// The turn that claimed them may have run gray already; re-running is
    /// the documented at-least-once behaviour.
    pub fn recover(&self) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let n = conn
            .execute("UPDATE inbox SET state='queued' WHERE state='running'", [])
            .map_err(map_err)?;
        Ok(n)
    }

    /// Chats with a turn in flight, for shutdown notices.
    pub fn running_channels(&self) -> Result<Vec<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id,channel,thread_ts,sender,conversation,text,state \
                 FROM inbox WHERE state='running' ORDER BY created",
            )
            .map_err(map_err)?;
        let rows = stmt.query_map([], row_to_item).map_err(map_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_err)
    }

    // ----- pairing codes -----

    /// Existing open code for this user, if any.
    pub fn pairing_code_for(&self, user_id: &str) -> Result<Option<String>, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row(
                "SELECT code FROM pairing WHERE user_id=?1 ORDER BY created DESC LIMIT 1",
                [user_id],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_err(other)),
            })
    }

    pub fn pairing_insert(&self, code: &str, user_id: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "INSERT OR REPLACE INTO pairing (code,user_id,created) VALUES (?1,?2,?3)",
                rusqlite::params![code, user_id, now_secs()],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Single-use consume: returns the user the code was minted for.
    pub fn pairing_take(&self, code: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let user: Option<String> = conn
            .query_row("SELECT user_id FROM pairing WHERE code=?1", [code], |r| {
                r.get(0)
            })
            .ok();
        if user.is_some() {
            conn.execute("DELETE FROM pairing WHERE code=?1", [code])
                .map_err(map_err)?;
        }
        Ok(user)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_err(other)),
            })
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "INSERT OR REPLACE INTO meta (key,value) VALUES (?1,?2)",
                rusqlite::params![key, value],
            )
            .map_err(map_err)?;
        Ok(())
    }
}
