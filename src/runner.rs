//! Isolated `gray -p --json` child per turn.
//! One conversation home per sha256(conversation); explicit session pointers
//! only — never guess a shared session id.
use serde_json::Value;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Cap on one NDJSON stdout line (matches gray's own wire limit).
const LINE_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub enum RunError {
    Busy,
    Timeout,
    /// The turn used all `max_requests` provider requests and was stopped.
    RequestLimit(u64),
    Spawn(String),
    Protocol(String),
    Exit(Option<i32>),
    Incomplete,
    Empty,
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("This conversation is busy"),
            Self::Timeout => f.write_str("gray turn timed out"),
            Self::RequestLimit(n) => write!(f, "gray turn hit its {n}-request limit"),
            Self::Spawn(_) => f.write_str("cannot start gray agent"),
            Self::Protocol(_) => {
                f.write_str("Invalid agent JSON output; upgrade gray to a compatible version")
            }
            Self::Exit(Some(n)) => write!(
                f,
                "gray exited with code {n}; not retrying possible side effects"
            ),
            Self::Exit(None) => {
                f.write_str("gray exited without a status; not retrying possible side effects")
            }
            Self::Incomplete => f.write_str(
                "Agent did not return a completed result; actions may already have occurred",
            ),
            Self::Empty => f.write_str("Agent returned no final answer"),
            Self::Io(_) => f.write_str("gray turn failed; check connectivity"),
        }
    }
}
impl std::error::Error for RunError {}

struct Consume {
    terminal: Option<Value>,
    protocol_ok: bool,
}

/// Held while a turn runs on a conversation home.
struct ConversationLock {
    _file: std::fs::File,
    fd: i32,
}

impl Drop for ConversationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
        }
    }
}

type SharedStdin = std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>;

fn live_stdins() -> &'static std::sync::Mutex<std::collections::HashMap<String, SharedStdin>> {
    static LIVE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, SharedStdin>>,
    > = std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
}

struct LiveStdin {
    conversation: String,
    stdin: SharedStdin,
}

impl LiveStdin {
    fn register(conversation: &str, stdin: SharedStdin) -> Self {
        if let Ok(mut live) = live_stdins().lock() {
            live.insert(conversation.to_string(), stdin.clone());
        }
        Self {
            conversation: conversation.to_string(),
            stdin,
        }
    }
}

impl Drop for LiveStdin {
    fn drop(&mut self) {
        if let Ok(mut live) = live_stdins().lock() {
            if live
                .get(&self.conversation)
                .is_some_and(|s| std::sync::Arc::ptr_eq(s, &self.stdin))
            {
                live.remove(&self.conversation);
            }
        }
    }
}

pub fn is_live(conversation: &str) -> bool {
    live_stdins()
        .lock()
        .map(|live| live.contains_key(conversation))
        .unwrap_or(false)
}

pub async fn steer(conversation: &str, text: &str) -> bool {
    use tokio::io::AsyncWriteExt;
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let Some(stdin) = live_stdins()
        .lock()
        .ok()
        .and_then(|live| live.get(conversation).cloned())
    else {
        return false;
    };
    let line = format!("{}\n", serde_json::json!({"type": "steer", "text": text}));
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await.is_ok() && stdin.flush().await.is_ok()
}

/// Single-quote shell escaping for launcher argv words.
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// gray's PATH inside the child: the real PATH plus the gray binary's dir
/// and the usual user locations, so a launchd/systemd PATH still finds it.
pub(crate) fn child_path(
    current: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    gray_bin: &Path,
) -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = gray_bin.parent() {
        dirs.push(dir.to_path_buf());
    }
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    let mut out: Vec<String> = dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    if let Some(current) = current {
        for part in std::env::split_paths(&current) {
            let s = part.to_string_lossy().into_owned();
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    if out.is_empty() {
        out.push("/usr/bin:/bin".to_string());
    }
    out.join(":")
}

/// Run one isolated text turn and return the agent's final answer.
pub async fn run_gray(
    config: &Value,
    config_path: &Path,
    conversation: &str,
    prompt: &str,
    timeout_secs: Option<u64>,
) -> Result<String, RunError> {
    let timeout_secs = match timeout_secs {
        Some(0) => return Err(RunError::Io("Timeout must be positive".to_string())),
        Some(n) => n,
        None => config
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(600),
    };
    if prompt.trim().is_empty() || prompt.chars().count() > 32000 {
        return Err(RunError::Io(
            "Prompt must contain 1-32000 characters".to_string(),
        ));
    }

    let home = crate::session::conversation_home(config_path, conversation);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .map_err(|_| RunError::Io("cannot create conversation home".to_string()))?;
    let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));

    // Non-blocking per-conversation lock, held for the whole turn.
    let lock_file = std::fs::File::create(home.join("run.lock"))
        .map_err(|_| RunError::Io("cannot lock conversation".to_string()))?;
    use std::os::unix::io::AsRawFd;
    let lock_fd = lock_file.as_raw_fd();
    // Do not let another concurrently spawned gray child inherit this lock
    // descriptor. Otherwise a child for conversation B can keep conversation
    // A's flock alive after A's runner future has returned.
    let fd_flags = unsafe { libc::fcntl(lock_fd, libc::F_GETFD) };
    if fd_flags < 0
        || unsafe { libc::fcntl(lock_fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(RunError::Io("cannot protect conversation lock".to_string()));
    }
    let locked = unsafe { libc::flock(lock_fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Err(RunError::Busy);
    }
    let _lock = ConversationLock {
        _file: lock_file,
        fd: lock_fd,
    };

    // Snapshot the provider config into the isolated home.
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| RunError::Io("gray provider configuration is missing".to_string()))?;
    crate::config::atomic_json(&home.join("config.json"), &provider).map_err(RunError::Io)?;

    // Dedicated workdir/profile: the slack tools plus the minimal builtin
    // set — no lockfile plugins that might start another gateway.
    let work = home.join("work");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&work)
        .or_else(|e| if work.is_dir() { Ok(()) } else { Err(e) })
        .map_err(|_| RunError::Io("cannot create work directory".to_string()))?;
    let _ = std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700));
    let launcher = home.join("slack-sidecar");
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "gray-slack".to_string());
    let abs_config = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    let script = format!(
        "#!/bin/sh\nexec {} sidecar --config {}\n",
        shell_word(&exe),
        shell_word(&abs_config)
    );
    std::fs::write(&launcher, script)
        .map_err(|_| RunError::Io("cannot write launcher".to_string()))?;
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| RunError::Io("cannot protect launcher".to_string()))?;
    let profile = format!(
        "plugins:\n  - builtin: tools-minimal\n  - sidecar: {}\n",
        serde_json::to_string(&launcher.to_string_lossy()).unwrap_or_default()
    );
    std::fs::write(work.join("gray.yml"), profile)
        .map_err(|_| RunError::Io("cannot write profile".to_string()))?;
    // Bound project discovery at the dedicated work directory.
    let _ = std::fs::create_dir(work.join(".git"));

    let state_path = home.join("session.json");
    let mut state: Value = std::fs::read(&state_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if !state.is_object() {
        state = Value::Object(Default::default());
    }
    let sessions = home.join("sessions");
    let has_session = state
        .get("session_id")
        .and_then(Value::as_str)
        .is_some_and(|sid| !sid.is_empty())
        || sessions
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    let now = crate::store::now_secs();
    if crate::session::ResetPolicy::from_config(config)
        .reset_reason(&state, has_session, now)
        .is_some()
    {
        crate::session::reset_home(&home, now).map_err(RunError::Io)?;
        state = serde_json::json!({
            "generation": crate::store::uuid_hex(),
            "last_activity": now
        });
    }
    if state
        .get("generation")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        state["generation"] = Value::String(crate::store::uuid_hex());
    }
    state["last_activity"] = serde_json::json!(now);
    crate::config::atomic_json(&state_path, &state).map_err(RunError::Io)?;
    let generation = state
        .get("generation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if gray_bin.is_empty() {
        return Err(RunError::Io("gray executable is missing".to_string()));
    }
    let max_requests = config
        .get("max_requests")
        .and_then(Value::as_u64)
        .unwrap_or(200);
    let mut args: Vec<String> = vec![
        gray_bin.to_string(),
        "-p".to_string(),
        prompt.to_string(),
        "--json".to_string(),
        "--max-requests".to_string(),
        max_requests.to_string(),
    ];
    let session_id = state
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(sid) = &session_id {
        args.push("--session".to_string());
        args.push(sid.clone());
    }

    let mut cmd = tokio::process::Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(&work);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    // Own process group so a timeout SIGKILL reaches grandchildren too.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k_str) = k.to_str() else { continue };
        if k_str.starts_with("GRAY_") || k_str.starts_with("SLACK_") || k_str.starts_with("OPENAI_")
        {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", &home);
    cmd.env(
        "PATH",
        child_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            Path::new(gray_bin),
        ),
    );
    cmd.env("GRAY_SKILLS_ONLY", "1");
    // Tool narration is safe to show; raw model reasoning is not.
    cmd.env("GRAY_SHOW_REASONING", "0");
    cmd.env("GRAY_MAX_WALL_SECS", (timeout_secs.max(1)).to_string());
    cmd.env("GRAY_STDIN_STEER", "1");

    let mut child = cmd
        .spawn()
        .map_err(|_| RunError::Spawn("spawn failed".to_string()))?;
    let pid = child.id().unwrap_or(0);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunError::Spawn("no stdout".to_string()))?;
    let _live = child.stdin.take().map(|s| {
        LiveStdin::register(
            conversation,
            std::sync::Arc::new(tokio::sync::Mutex::new(s)),
        )
    });
    let consume = consume_ndjson(stdout, &state_path, &generation, &mut state);
    let outcome: Result<Consume, RunError> =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), consume).await {
            Ok(r) => r,
            Err(_) => {
                // Timeout: kill the whole process group.
                if pid != 0 {
                    unsafe {
                        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
                let _ = child.wait().await;
                return Err(RunError::Timeout);
            }
        };
    let status = child
        .wait()
        .await
        .map_err(|_| RunError::Spawn("wait failed".to_string()))?;
    let consumed = outcome?;

    // gray reports its request cap as a generic error row; the accounting
    // beside it says whether the cap is what stopped the turn.
    if let Some(row) = consumed.terminal.as_ref().filter(|r| r["type"] == "error") {
        if row["accounting"]["requests"].as_u64() >= Some(max_requests) {
            return Err(RunError::RequestLimit(max_requests));
        }
    }
    if !status.success() {
        return Err(RunError::Exit(status.code()));
    }
    if !consumed.protocol_ok {
        return Err(RunError::Protocol(
            "Invalid agent JSON output; upgrade gray to a compatible version".to_string(),
        ));
    }
    let Some(final_row) = consumed.terminal else {
        return Err(RunError::Incomplete);
    };
    if final_row.get("type").and_then(Value::as_str) != Some("result")
        || final_row
            .get("session_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(RunError::Incomplete);
    }
    let answer = match final_row.get("text").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => Ok(t.to_string()),
        _ => Err(RunError::Empty),
    };
    // Drop the lock before the async state machine is suspended again.
    drop(_lock);
    answer
}

/// Drain the child's NDJSON stdout: protocol 1 rows, one turn, terminal
/// `result`/`error`, session pointer pinned into `state`.
async fn consume_ndjson(
    stdout: tokio::process::ChildStdout,
    state_path: &Path,
    generation: &str,
    state: &mut Value,
) -> Result<Consume, RunError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    let mut turn_id: Option<String> = None;
    let mut terminal: Option<Value> = None;
    let mut protocol_ok = true;
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|_| RunError::Io("cannot read agent output".to_string()))?;
        let Some(line) = line else { break };
        if line.len() > LINE_CAP {
            protocol_ok = false;
            continue;
        }
        let row: Value = match serde_json::from_str(&line) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => {
                protocol_ok = false;
                continue;
            }
        };
        if row.get("type").and_then(Value::as_str) == Some("retract") {
            continue;
        }
        if row.get("protocol").and_then(Value::as_i64) != Some(1) {
            protocol_ok = false;
            continue;
        }
        let row_turn = row.get("turn_id").and_then(Value::as_str).unwrap_or("");
        if row_turn.is_empty() || (turn_id.is_some() && turn_id.as_deref() != Some(row_turn)) {
            protocol_ok = false;
            continue;
        }
        turn_id = Some(row_turn.to_string());
        // Data after the terminal row is a protocol violation, not a
        // second result.
        if terminal.is_some() {
            protocol_ok = false;
            continue;
        }
        if let Some(sid) = row.get("session_id").and_then(Value::as_str) {
            if session_id_valid(sid) {
                // `/new` advances the on-disk generation while an old child
                // may still be finishing. Never let that child resurrect its
                // predecessor's pointer.
                if !crate::session::generation_is_current(state_path, generation) {
                    protocol_ok = false;
                    continue;
                }
                let pinned = state
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !pinned.is_empty() && pinned != sid {
                    protocol_ok = false;
                    continue;
                }
                state["session_id"] = Value::String(sid.to_string());
                let _ = crate::config::atomic_json(state_path, state);
            } else {
                protocol_ok = false;
                continue;
            }
        }
        match row.get("type").and_then(Value::as_str) {
            Some("result") | Some("error") => {
                terminal = Some(row);
            }
            Some("progress") | Some("text") => {}
            _ => {
                protocol_ok = false;
            }
        }
    }
    Ok(Consume {
        terminal,
        protocol_ok,
    })
}

fn session_id_valid(s: &str) -> bool {
    // gray names sessions with readable lowercase words
    // (`ionic-nickel-supernova`); older releases emitted canonical UUIDs.
    // Both are dash-separated lowercase [a-z0-9] runs; keep the check
    // structural so a word id never trips a protocol error.
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
