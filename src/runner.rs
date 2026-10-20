//! Isolated `gray -p --json` child per turn.
//! One conversation home per sha256(conversation); explicit session pointers
//! only — never guess a shared session id.
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Cap on one NDJSON stdout line (matches gray's own wire limit).
const LINE_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub enum RunError {
    Busy,
    Timeout(u64),
    /// The turn used all `max_requests` provider requests and was stopped.
    RequestLimit(u64),
    Provider(String),
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
            Self::Busy => f.write_str("this conversation is busy"),
            Self::Timeout(secs) => write!(f, "the turn ran past its {} limit", human_secs(*secs)),
            Self::RequestLimit(n) => write!(f, "the turn used all {n} of its provider requests"),
            Self::Provider(reason) => write!(f, "the agent reported an error: {reason}"),
            Self::Spawn(_) => f.write_str("cannot start gray agent"),
            Self::Protocol(_) => f.write_str(
                "gray sent output this plugin cannot read; upgrade gray to a compatible version",
            ),
            Self::Exit(Some(n)) => write!(f, "gray exited with code {n}"),
            Self::Exit(None) => f.write_str("gray exited without a status"),
            Self::Incomplete => f.write_str("the agent did not finish its reply"),
            Self::Empty => f.write_str("the agent returned no final answer"),
            Self::Io(reason) => f.write_str(&reason.to_lowercase()),
        }
    }
}
impl std::error::Error for RunError {}

pub fn failure_notice(err: &RunError) -> String {
    format!(
        "That failed: {}. Some steps may already have run. Send a message to try again.",
        plain_reason(&err.to_string())
    )
}

fn plain_reason(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(300) {
        Some((cut, _)) => format!("{}…", &collapsed[..cut]),
        None => collapsed,
    }
}

fn human_secs(secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn error_row_reason(row: &Value) -> Option<String> {
    row.pointer("/error/message")
        .or_else(|| row.get("message"))
        .or_else(|| row.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

struct Consume {
    terminal: Option<Value>,
    protocol_ok: bool,
}

/// Held while a turn runs on a conversation home.
struct ConversationLock {
    _file: std::fs::File,
}

impl Drop for ConversationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

struct ProcessGroup(u32);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.0 > 0 {
            kill_process_tree(self.0);
        }
    }
}

/// Exclusive lock on `path`: `Some(file)` while another process has not
/// already locked it. Unix takes `flock` on an FD_CLOEXEC descriptor;
/// Windows opens with `share_mode(0)`, which closes the lock with the file.
#[cfg(unix)]
pub(crate) fn open_lock_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::create(path)?;
    let fd = f.as_raw_fd();
    // Do not let another concurrently spawned gray child inherit this lock
    // descriptor.
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fd_flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    Ok(locked.then_some(f))
}

#[cfg(windows)]
pub(crate) fn open_lock_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(0)
        .open(path)
    {
        Ok(f) => Ok(Some(f)),
        // ERROR_SHARING_VIOLATION: another process holds the lock.
        Err(e) if e.raw_os_error() == Some(32) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Kill the process tree rooted at `pid`: the child's process group on unix
/// (it leads its own group, see `pre_exec`), `taskkill /T` on Windows.
#[cfg(unix)]
pub(crate) fn kill_process_tree(pid: u32) {
    // SAFETY: kill only signals.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", pid.to_string().as_str(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
#[cfg(unix)]
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
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&home)
        .map_err(|_| RunError::Io("cannot create conversation home".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }

    // Non-blocking per-conversation lock, held for the whole turn.
    let Some(lock_file) = open_lock_file(&home.join("run.lock"))
        .map_err(|_| RunError::Io("cannot lock conversation".to_string()))?
    else {
        return Err(RunError::Busy);
    };
    let _lock = ConversationLock { _file: lock_file };

    // Snapshot the provider config into the isolated home.
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut provider: Value = std::fs::read(Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| RunError::Io("gray provider configuration is missing".to_string()))?;
    if let Some(model) = config
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
    {
        provider["model"] = Value::String(model.to_string());
    }
    crate::config::atomic_json(&home.join("config.json"), &provider).map_err(RunError::Io)?;

    // Dedicated workdir/profile: the slack tools plus the minimal builtin
    // set — no lockfile plugins that might start another gateway.
    let work = home.join("work");
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&work)
        .or_else(|e| if work.is_dir() { Ok(()) } else { Err(e) })
        .map_err(|_| RunError::Io("cannot create work directory".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(windows)]
    let launcher = home.join("slack-sidecar.cmd");
    #[cfg(unix)]
    let launcher = home.join("slack-sidecar");
    // A reinstall under the running service reads as "<path> (deleted)";
    // the launcher must exec the replacement at <path>.
    let exe = std::env::current_exe()
        .map(|p| {
            let p = p.to_string_lossy();
            p.strip_suffix(" (deleted)").unwrap_or(&p).to_string()
        })
        .unwrap_or_else(|_| "gray-slack".to_string());
    let abs_config = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    #[cfg(unix)]
    let script = format!(
        "#!/bin/sh\nexec {} sidecar --config {}\n",
        shell_word(&exe),
        shell_word(&abs_config)
    );
    #[cfg(windows)]
    let script = format!(
        "@echo off\r\n\"{}\" sidecar --config \"{}\"\r\n",
        exe, abs_config
    );
    std::fs::write(&launcher, script)
        .map_err(|_| RunError::Io("cannot write launcher".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| RunError::Io("cannot protect launcher".to_string()))?;
    }
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
    // Own process group so a timeout kill reaches grandchildren too.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
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
    if !gray_home.is_empty() {
        cmd.env("GRAY_USER_HOME", gray_home);
    }
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
    let mut group = ProcessGroup(pid);

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
                    kill_process_tree(pid);
                }
                let _ = child.wait().await;
                group.0 = 0;
                return Err(RunError::Timeout(timeout_secs));
            }
        };
    let status = child.wait().await;
    group.0 = 0;
    let status = status.map_err(|_| RunError::Spawn("wait failed".to_string()))?;
    let consumed = outcome?;

    // gray reports its request cap as a generic error row; the accounting
    // beside it says whether the cap is what stopped the turn.
    if let Some(row) = consumed.terminal.as_ref().filter(|r| r["type"] == "error") {
        if row["accounting"]["requests"].as_u64() >= Some(max_requests) {
            return Err(RunError::RequestLimit(max_requests));
        }
        if let Some(reason) = error_row_reason(row) {
            return Err(RunError::Provider(reason));
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

pub async fn cancellable<F: std::future::Future>(
    fut: F,
    cancelled: impl Fn() -> bool,
) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            _ = tick.tick() => {
                if cancelled() {
                    return None;
                }
            }
        }
    }
}
