//! Interactive setup wizard.
//! Credentials stay in the wizard; the model never receives the tokens.
use serde_json::{json, Value};
use std::path::Path;

use crate::transport::Rest;

/// Slack rejections the wizard re-asks before giving up.
pub const TOKEN_TRIES: usize = 3;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Real terminal prompter (hidden input via `stty -echo`, no new deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Echo off for one read via the console input mode. If setup is Ctrl-C'd
/// mid-read the console stays silent, but cmd.exe restores its own mode at
/// the next prompt.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return read();
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return read();
        }
        let saved = mode;
        SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT);
        let out = read();
        SetConsoleMode(handle, saved);
        out
    }
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains('/') || candidate.contains('\\') {
        let path = Path::new(candidate);
        if path.is_file() && is_executable(path) {
            return Some(candidate.to_string());
        }
        return None;
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let mut candidates = vec![dir.join(candidate)];
        if cfg!(windows) && !candidate.ends_with(".exe") {
            candidates.push(dir.join(format!("{candidate}.exe")));
        }
        for full in candidates {
            if full.is_file() && is_executable(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn shellexpand_home(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let home = Path::new(&home).to_string_lossy().into_owned();
            return if raw == "~" {
                home
            } else {
                format!("{home}/{}", &raw[2..])
            };
        }
    }
    raw.to_string()
}

/// Curly quotes and other non-ASCII glyphs from a rich-text paste are
/// stripped before the check and before the save.
pub fn clean_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii() && !c.is_whitespace() && *c != '\0')
        .collect()
}

/// `owner_id` and `allowed_users` from the config being replaced, read raw
/// (it may not validate any more). Missing or unreadable means nobody.
fn prior_access(old: Option<&Value>) -> (Option<String>, Vec<String>, Vec<String>) {
    let Some(old) = old else {
        return (None, Vec::new(), Vec::new());
    };
    let list = |key: &str| -> Vec<String> {
        old.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    (
        old.get("owner_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        list("allowed_users"),
        list("allowed_channels"),
    )
}

fn merge(existing: Vec<String>, new: &[String]) -> Vec<String> {
    let mut out = existing;
    for id in new {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// Ask until auth.test accepts a bot token (or the room runs out of tries).
/// `Ok((token, None))` means Slack could not be reached: kept, with a warning.
async fn prompt_bot_token(
    io: &mut dyn Prompter,
    app: &str,
) -> Result<(String, Option<crate::transport::Auth>), String> {
    // An exported SLACK_BOT_TOKEN answers the question without a paste.
    let env = clean_token(&std::env::var("SLACK_BOT_TOKEN").unwrap_or_default());
    let mut tries = 0;
    loop {
        let token = if !env.is_empty() && tries == 0 {
            env.clone()
        } else {
            clean_token(&io.prompt_hidden("Slack bot token, xoxb-... (hidden): ")?)
        };
        if token.is_empty() {
            return Err("A bot token is required".to_string());
        }
        if !token.starts_with("xoxb-") {
            io.print_line("That doesn't look like a bot token (xoxb-…). Paste the Bot User OAuth Token from OAuth & Permissions.");
            tries += 1;
            if tries >= TOKEN_TRIES {
                return Err("No working bot token after three tries".to_string());
            }
            continue;
        }
        let rest = Rest::production(&token, app);
        match rest.auth_test().await {
            Ok(auth) => {
                io.print_line(&format!(
                    "Token works: the bot is @{} in workspace \"{}\".",
                    auth.user, auth.team
                ));
                return Ok((token, Some(auth)));
            }
            Err(crate::transport::SlackError::Api(e)) => {
                io.print_line(&format!(
                    "Slack rejected that token ({e}). Check OAuth & Permissions and paste it again."
                ));
                tries += 1;
                if !env.is_empty() {
                    // An env token that failed is not asked again by hand.
                    return Err("SLACK_BOT_TOKEN was rejected by Slack".to_string());
                }
                if tries >= TOKEN_TRIES {
                    return Err("No working bot token after three tries".to_string());
                }
            }
            Err(crate::transport::SlackError::Transport(_)) => {
                io.print_line("Slack could not be reached; keeping the token unverified.");
                return Ok((token, None));
            }
        }
    }
}

/// The app-level token: verified by actually asking for a socket URL.
async fn prompt_app_token(io: &mut dyn Prompter, bot: &str) -> Result<String, String> {
    let env = clean_token(&std::env::var("SLACK_APP_TOKEN").unwrap_or_default());
    let mut tries = 0;
    loop {
        let token = if !env.is_empty() && tries == 0 {
            env.clone()
        } else {
            clean_token(&io.prompt_hidden("Slack app token, xapp-... (hidden): ")?)
        };
        if token.is_empty() {
            return Err("An app-level token is required for Socket Mode".to_string());
        }
        if !token.starts_with("xapp-") {
            io.print_line("That doesn't look like an app token (xapp-…). Basic Information → App-Level Tokens, scope connections:write.");
            tries += 1;
            if tries >= TOKEN_TRIES {
                return Err("No working app token after three tries".to_string());
            }
            continue;
        }
        let rest = Rest::production(bot, &token);
        match rest.connections_open().await {
            Ok(_) => {
                io.print_line("App token works: Socket Mode answered with a connection URL.");
                return Ok(token);
            }
            Err(crate::transport::SlackError::Api(e)) => {
                io.print_line(&format!(
                    "Slack rejected that app token ({e}). It needs the connections:write scope."
                ));
                tries += 1;
                if !env.is_empty() {
                    return Err("SLACK_APP_TOKEN was rejected by Slack".to_string());
                }
                if tries >= TOKEN_TRIES {
                    return Err("No working app token after three tries".to_string());
                }
            }
            Err(crate::transport::SlackError::Transport(_)) => {
                io.print_line("Slack could not be reached; keeping the app token unverified.");
                return Ok(token);
            }
        }
    }
}

/// conversations.open — the DM channel with a user (setup's home channel).
async fn dm_channel(rest: &Rest, user: &str) -> Result<String, String> {
    let data = rest
        .conversations_open(user)
        .await
        .map_err(|e| format!("cannot open a DM with that member ({e})"))?;
    Ok(data)
}

/// The wizard.
pub async fn run(path: &Path, io: &mut dyn Prompter) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("Setup needs a terminal for hidden token input".to_string());
    }
    if path.exists() && !io.confirm("Replace existing configuration? [y/N] ")? {
        return Ok(false);
    }
    let gray = resolve_gray(&std::env::var("GRAY_BIN").unwrap_or_else(|_| "gray".to_string()))
        .ok_or_else(|| "Install gray first; executable not found".to_string())?;
    let gray_home = absolutize(Path::new(&shellexpand_home(
        &std::env::var("GRAY_HOME").unwrap_or_else(|_| "~/.gray".to_string()),
    )));

    let prior = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object);
    let (prior_owner, prior_users, prior_channels) = prior_access(prior.as_ref());

    for line in [
        "1. Open https://api.slack.com/apps → Create New App → From scratch",
        "2. Settings → Socket Mode → on, then create an App-Level Token (xapp-…) with scope connections:write",
        "3. OAuth & Permissions → Bot Token Scopes: chat:write, files:write, channels:history, groups:history, im:history, mpim:history, app_mentions:read → Install to Workspace → copy the xoxb-… token",
        "4. Features → Event Subscriptions → on; Subscribe to bot events: message.channels, message.groups, message.im, message.mpim",
        "Both tokens are checked with Slack before anything is saved.",
    ] {
        io.print_line(line);
    }

    let (bot_token, auth) = prompt_bot_token(io, "").await?;
    let app_token = prompt_app_token(io, &bot_token).await?;

    // Who may talk to the bot. A prior owner is offered again.
    let owner = match &prior_owner {
        Some(o) if io.confirm(&format!("Keep owner {o}? [Y/n] "))? => o.clone(),
        _ => {
            let answer = io.prompt(
                "Your Slack member ID (profile → ⋯ → Copy member ID; comma-separated to also allow others): ",
            )?;
            let ids: Vec<String> = answer
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if ids.is_empty()
                || !ids
                    .iter()
                    .all(|id| crate::config::user_id(&Value::String(id.clone())))
            {
                return Err("Member IDs look like U0123ABCDEF".to_string());
            }
            ids[0].clone()
        }
    };
    let extra = io.prompt("Other allowed member IDs (comma-separated, Enter to skip): ")?;
    let extras: Vec<String> = extra
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let allowed_users = merge(
        prior_users,
        &extras
            .iter()
            .filter(|id| crate::config::user_id(&Value::String(id.to_string())))
            .cloned()
            .collect::<Vec<_>>(),
    );

    // Home channel: the bot's DM with the owner, like the Discord bridge.
    let rest = Rest::production(&bot_token, &app_token);
    let home = dm_channel(&rest, &owner).await?;
    io.print_line(&format!("Home channel: your DM with the bot ({home})"));

    let extra_channels = io.prompt(
        "Other allowed channel IDs the bot should answer in (comma-separated, Enter to skip): ",
    )?;
    let channel_extras: Vec<String> = extra_channels
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let allowed_channels = merge(
        prior_channels,
        &channel_extras
            .iter()
            .filter(|id| crate::config::channel_id(&Value::String(id.to_string())))
            .cloned()
            .collect::<Vec<_>>(),
    );

    let workdir = path
        .parent()
        .map(|p| absolutize(p).to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let mut saved = json!({
        "bot_token": bot_token,
        "app_token": app_token,
        "owner_id": owner,
        "channel_id": home,
        "bot_user_id": auth.as_ref().map(|a| a.user_id.clone()).unwrap_or_default(),
        "team_id": auth.as_ref().map(|a| a.team_id.clone()).unwrap_or_default(),
        "gray_bin": absolutize(Path::new(&gray)).to_string_lossy(),
        "gray_home": gray_home.to_string_lossy(),
        "workdir": workdir,
        "reply_in_thread": true,
        "session_reset": {
            "mode": "both",
            "idle_minutes": 1_440,
            "at_hour": 4
        }
    });
    if !allowed_users.is_empty() {
        saved["allowed_users"] =
            Value::Array(allowed_users.into_iter().map(Value::String).collect());
    }
    if !allowed_channels.is_empty() {
        saved["allowed_channels"] =
            Value::Array(allowed_channels.into_iter().map(Value::String).collect());
    }
    // A re-run replaces only what the wizard asked about: limits and other
    // hand-tuned keys survive.
    if let Some(mut merged) = prior {
        for (k, v) in saved.as_object().into_iter().flatten() {
            if k != "session_reset" || merged.get(k).is_none() {
                merged[k] = v.clone();
            }
        }
        for key in ["allowed_users", "allowed_channels"] {
            if saved.get(key).is_none() {
                if let Some(m) = merged.as_object_mut() {
                    m.remove(key);
                }
            }
        }
        if crate::config::validate_config(&merged).is_ok() {
            saved = merged;
        }
    }
    crate::config::save_config(path, &saved)?;
    io.print_line("Configuration saved privately.");

    // Prove it with the app's own doctor before anyone is told it worked.
    crate::doctor::doctor(&saved).await.map_err(|e| {
        format!("Saved, but the doctor disagreed: {e}. Fix it, then run `gray slack doctor` and `gray slack install`.")
    })?;
    io.print_line("Doctor verified the configuration.");
    Ok(true)
}
