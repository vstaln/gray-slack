//! Slack conversation identity and session reset policy.
//!
//! A Slack thread is already a separate session: the conversation key folds
//! in the thread's root ts. Non-DM channel messages additionally key on the
//! user, so people in one channel don't share a transcript through the bot.
use chrono::{DateTime, Local, TimeZone, Timelike};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResetReason {
    /// A pre-policy session had no activity timestamp. It is reset once when
    /// an enabled policy first sees it, then follows the normal policy.
    Legacy,
    Idle,
    Daily,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetPolicy {
    pub mode: String,
    pub idle_minutes: u64,
    pub at_hour: u32,
}

impl Default for ResetPolicy {
    fn default() -> Self {
        Self {
            mode: "both".into(),
            idle_minutes: 1_440,
            at_hour: 4,
        }
    }
}

impl ResetPolicy {
    pub fn from_config(config: &Value) -> Self {
        let raw = config.get("session_reset");
        let mut out = Self::default();
        if let Some(policy) = raw {
            if let Some(mode) = policy.get("mode").and_then(Value::as_str) {
                out.mode = mode.to_string();
            }
            if let Some(minutes) = policy.get("idle_minutes").and_then(Value::as_u64) {
                out.idle_minutes = minutes;
            }
            if let Some(hour) = policy.get("at_hour").and_then(Value::as_u64) {
                out.at_hour = hour as u32;
            }
        }
        out
    }

    /// Whether the current private home should start fresh before this turn.
    pub fn reset_reason(&self, state: &Value, has_session: bool, now: f64) -> Option<ResetReason> {
        if self.mode == "none" || !has_session {
            return None;
        }
        let Some(last) = state.get("last_activity").and_then(Value::as_f64) else {
            return Some(ResetReason::Legacy);
        };
        if matches!(self.mode.as_str(), "idle" | "both")
            && (now - last).max(0.0) >= self.idle_minutes.saturating_mul(60) as f64
        {
            return Some(ResetReason::Idle);
        }
        if matches!(self.mode.as_str(), "daily" | "both") && self.daily_boundary_passed(last, now) {
            return Some(ResetReason::Daily);
        }
        None
    }

    fn daily_boundary_passed(&self, last: f64, now: f64) -> bool {
        let Some(now_local) = local_at(now) else {
            return false;
        };
        let Some(last_local) = local_at(last) else {
            return false;
        };
        let today = now_local.date_naive();
        let boundary_date = if now_local.hour() >= self.at_hour {
            today
        } else {
            today - chrono::Duration::days(1)
        };
        let Some(boundary) = boundary_date.and_hms_opt(self.at_hour, 0, 0) else {
            return false;
        };
        last_local.naive_local() < boundary
    }
}

fn local_at(timestamp: f64) -> Option<DateTime<Local>> {
    if !timestamp.is_finite() || timestamp < i64::MIN as f64 || timestamp > i64::MAX as f64 {
        return None;
    }
    Local
        .timestamp_opt(timestamp as i64, 0)
        .single()
        .or_else(|| Local.timestamp_opt(timestamp as i64, 0).earliest())
}

/// Build the session key for one inbound surface.
///
/// `channel` is the Slack conversation id; `thread_ts` the thread root the
/// message belongs to ("" = top level). DM channels stay keyed by channel;
/// threaded channel messages key on the thread; top-level channel messages
/// add the user id, the secure group default.
pub fn conversation_key(channel: &str, thread_ts: &str, user_id: &str, is_dm: bool) -> String {
    if is_dm || user_id.trim().is_empty() {
        format!("chat:{channel}")
    } else if !thread_ts.is_empty() {
        format!("chat:{channel}:thread:{thread_ts}")
    } else {
        format!("chat:{channel}:user:{user_id}")
    }
}

/// Forget the transcript in one private conversation home and leave a fresh
/// generation marker behind. The marker prevents a turn that was already in
/// flight during `/new` from restoring its old session pointer when it exits.
pub fn reset_home(home: &Path, now: f64) -> Result<(), String> {
    let sessions = home.join("sessions");
    let mut transcripts = Vec::new();
    if sessions.is_dir() {
        list_jsonl(&sessions, Path::new(""), &mut transcripts)?;
    }
    if !transcripts.is_empty() {
        let archive = archive_dir(home, now)?;
        for rel in &transcripts {
            archive_file(&sessions.join(rel), &archive.join(rel))?;
        }
    }
    let state = json!({
        "generation": crate::store::uuid_hex(),
        "last_activity": now
    });
    crate::config::atomic_json(&home.join("session.json"), &state)
}

fn list_jsonl(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|_| "cannot list conversation sessions".to_string())?;
    for entry in entries {
        let entry = entry.map_err(|_| "cannot list conversation sessions".to_string())?;
        let file_type = entry
            .file_type()
            .map_err(|_| "cannot inspect conversation session".to_string())?;
        let name = entry.file_name();
        if file_type.is_dir() {
            list_jsonl(&entry.path(), &rel.join(&name), out)?;
        } else if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            out.push(rel.join(&name));
        }
    }
    Ok(())
}

fn archive_dir(home: &Path, now: f64) -> Result<PathBuf, String> {
    let root = home.join("sessions-archive");
    std::fs::create_dir_all(&root)
        .map_err(|_| "cannot archive conversation session".to_string())?;
    let stamp = local_at(now)
        .ok_or_else(|| "cannot archive conversation session".to_string())?
        .format("%Y-%m-%d-%H%M%S")
        .to_string();
    let mut candidate = root.join(&stamp);
    let mut n = 2;
    loop {
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                candidate = root.join(format!("{stamp}-{n}"));
                n += 1;
            }
            Err(_) => return Err("cannot archive conversation session".to_string()),
        }
    }
}

fn archive_file(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "cannot archive conversation session".to_string())?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map_err(|_| "cannot archive conversation session".to_string())?;
    std::fs::remove_file(from).map_err(|_| "cannot clear conversation session".to_string())
}

/// True when the on-disk state still belongs to the same conversation
/// generation. A reset advances this value while the old child is running.
pub fn generation_is_current(state_path: &Path, generation: &str) -> bool {
    std::fs::read(state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|state| {
            state
                .get("generation")
                .and_then(Value::as_str)
                .map(|current| current == generation)
        })
        .unwrap_or(false)
}

/// The directory one conversation calls home.
pub fn conversation_home(config_path: &Path, conversation: &str) -> PathBuf {
    use sha2::Digest;
    let key = format!("{:x}", sha2::Sha256::digest(conversation.as_bytes()));
    config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("conversations")
        .join(key)
}
