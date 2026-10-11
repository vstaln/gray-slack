//! Headless `gray cmd` behind a text prefix: `!name args`, `/name args`,
//! or `<@bot> /name args` once the mention is stripped.
//!
//! Slack slash commands need app-console wiring this plugin cannot touch,
//! so the trigger is a plain prefix in text the socket already sees. A name
//! is only a command when the binary advertises it (`gray cmd list`, cached
//! five minutes); an old binary without `cmd` falls back to a static
//! allowlist, and anything unknown falls through to a normal turn — command
//! typos reach the model rather than dying silently.
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `gray cmd list` answers are reused this long.
const LIST_TTL: Duration = Duration::from_secs(300);
/// Ceiling on any one `gray cmd` child.
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Registry rows marked `slow` (compaction, plugin calls, updates) get
/// this window instead — a posted reply has no platform deadline.
const SLOW_TIMEOUT: Duration = Duration::from_secs(120);

/// Names assumed present when `gray cmd list` cannot be read — an old
/// binary, or a spawn failure. All conversation-scoped.
const STATIC_NAMES: &[&str] = &[
    "help", "model", "usage", "new", "sessions", "cron", "skills",
];

/// The contract's `scope`: which home a command runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Conversation,
    Global,
}

/// What the registry knows about a command: where it runs and whether it
/// can outlast the normal timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmd {
    pub scope: Scope,
    pub slow: bool,
}

/// `!name args` / `/name args` -> `(name, args)`.
///
/// A leading `<@bot>` mention is stripped first (the daemon already strips
/// mentions before dispatch; doing it again keeps this self-contained).
/// The name must match `[a-z][a-z0-9_-]*` and args must be
/// whitespace-separated: `/etc/hosts` and `!a.b` stay plain text.
pub fn parse_prefixed(text: &str, bot_user_id: &str) -> Option<(String, String)> {
    let mut rest = text.trim();
    if !bot_user_id.is_empty() {
        let mention = format!("<@{bot_user_id}>");
        while let Some(r) = rest.strip_prefix(mention.as_str()) {
            rest = r.trim_start_matches(|c: char| c.is_whitespace() || c == ':' || c == ',');
        }
    }
    let rest = rest.strip_prefix('!').or_else(|| rest.strip_prefix('/'))?;
    let name_len = rest
        .find(|c: char| {
            !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
        .unwrap_or(rest.len());
    let (name, args) = rest.split_at(name_len);
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return None;
    }
    if !args.is_empty() && !args.starts_with(char::is_whitespace) {
        return None;
    }
    Some((name.to_string(), args.trim().to_string()))
}

/// Parse + verify: the command to run, or `None` when the text names
/// nothing the registry knows — it stays a normal prompt.
pub fn dispatch(
    text: &str,
    bot_user_id: &str,
    names: &HashMap<String, Cmd>,
) -> Option<(String, String, Cmd)> {
    let (name, args) = parse_prefixed(text, bot_user_id)?;
    let cmd = *names.get(&name)?;
    Some((name, args, cmd))
}

/// The home a command runs against: this conversation, or the user's main
/// `gray_home` for global-scope commands (conversation home when the
/// config names none).
pub fn command_home(
    config: &Value,
    config_path: &Path,
    conversation: &str,
    scope: Scope,
) -> PathBuf {
    if scope == Scope::Global {
        if let Some(home) = config.get("gray_home").and_then(Value::as_str) {
            if !home.is_empty() {
                return PathBuf::from(home);
            }
        }
    }
    crate::session::conversation_home(config_path, conversation)
}

/// Five-minute `gray cmd list` cache; falls back to the static allowlist.
pub struct Cache(Mutex<Option<Cached>>);

struct Cached {
    names: HashMap<String, Cmd>,
    fetched: Instant,
}

impl Cache {
    pub fn new() -> Self {
        Cache(Mutex::new(None))
    }

    /// Name -> scope/speed for every command the binary advertises,
    /// canonical names and aliases alike; the static allowlist when `list`
    /// fails. A failure caches the fallback too, so a stale binary is
    /// re-probed on the TTL rather than per message.
    pub async fn names(&self, gray_bin: &Path, home: &Path) -> HashMap<String, Cmd> {
        {
            let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = guard.as_ref() {
                if cached.fetched.elapsed() < LIST_TTL {
                    return cached.names.clone();
                }
            }
        }
        let names = list(gray_bin, home).await.unwrap_or_else(static_names);
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Cached {
            names: names.clone(),
            fetched: Instant::now(),
        });
        names
    }
}

fn static_names() -> HashMap<String, Cmd> {
    STATIC_NAMES
        .iter()
        .map(|n| {
            (
                n.to_string(),
                Cmd {
                    scope: Scope::Conversation,
                    slow: false,
                },
            )
        })
        .collect()
}

/// `gray cmd list` -> name/alias -> scope. Any failure is `None`; the
/// caller swaps in the static allowlist.
async fn list(gray_bin: &Path, home: &Path) -> Option<HashMap<String, Cmd>> {
    ensure_home(home).ok()?;
    let out = cmd_output(gray_bin, home, &["list"], CMD_TIMEOUT)
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let reply: Value = serde_json::from_slice(&out.stdout).ok()?;
    parse_list(&reply)
}

/// `result.commands[]` -> the verification map.
fn parse_list(reply: &Value) -> Option<HashMap<String, Cmd>> {
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let commands = reply.pointer("/result/commands").and_then(Value::as_array)?;
    let mut names = HashMap::new();
    for command in commands {
        let row = Cmd {
            scope: match command.get("scope").and_then(Value::as_str) {
                Some("global") => Scope::Global,
                _ => Scope::Conversation,
            },
            slow: command.get("slow").and_then(Value::as_bool).unwrap_or(false),
        };
        if let Some(name) = command.get("name").and_then(Value::as_str) {
            names.insert(name.to_string(), row);
        }
        for alias in command
            .get("aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            names.insert(alias.to_string(), row);
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// What a `cmd run` answer becomes: text to post, or a prompt the caller
/// queues as a turn in this conversation.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Text(String),
    Prompt { prompt: String, fallback: String },
}

/// `gray cmd run <name> [args]` in `home`. `kind:"text"` answers the
/// markdown body; `kind:"ui"` — Slack v1 has no components — answers the
/// `fallback_text` render; `kind:"prompt"` hands back the turn text;
/// `!ok` surfaces the wire's error message.
pub async fn run(
    gray_bin: &Path,
    home: &Path,
    name: &str,
    args: &str,
    slow: bool,
) -> Result<Outcome, String> {
    ensure_home(home)?;
    let mut argv = vec!["run", name];
    if !args.is_empty() {
        argv.push(args);
    }
    let timeout = if slow { SLOW_TIMEOUT } else { CMD_TIMEOUT };
    let out = cmd_output(gray_bin, home, &argv, timeout).await?;
    if let Ok(reply) = serde_json::from_slice::<Value>(&out.stdout) {
        return result_text(&reply);
    }
    Err(match out.status.code() {
        Some(code) => format!("gray cmd exited with code {code}"),
        None => "gray cmd failed".to_string(),
    })
}

/// Wire reply -> what to do with it (stdout carries one JSON object).
fn result_text(reply: &Value) -> Result<Outcome, String> {
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        let message = reply
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| reply.pointer("/error/code").and_then(Value::as_str))
            .unwrap_or("command failed");
        return Err(message.to_string());
    }
    let result = reply.get("result").unwrap_or(&Value::Null);
    if result.get("kind").and_then(Value::as_str) == Some("prompt") {
        let prompt = result
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if prompt.trim().is_empty() {
            return Err("gray cmd answered an empty prompt".to_string());
        }
        let fallback = result
            .get("fallback_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(Outcome::Prompt { prompt, fallback });
    }
    let field = match result.get("kind").and_then(Value::as_str) {
        Some("text") => "text",
        Some("ui") => "fallback_text",
        _ => return Err("unreadable gray cmd result".to_string()),
    };
    let text = result.get(field).and_then(Value::as_str).unwrap_or("");
    Ok(Outcome::Text(if text.trim().is_empty() {
        "Done.".to_string()
    } else {
        text.to_string()
    }))
}

/// One `gray cmd` child with the runner's env discipline: cleared
/// environment minus the GRAY_/SLACK_/OPENAI_ leak set, GRAY_HOME scoped,
/// PATH rebuilt so a sparse service PATH still finds helpers.
async fn cmd_output(
    gray_bin: &Path,
    home: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new(gray_bin);
    cmd.arg("cmd");
    cmd.args(args);
    let work = home.join("work");
    cmd.current_dir(if work.is_dir() { &work } else { home });
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k_str) = k.to_str() else { continue };
        if k_str.starts_with("GRAY_") || k_str.starts_with("SLACK_") || k_str.starts_with("OPENAI_")
        {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", home);
    cmd.env(
        "PATH",
        crate::runner::child_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            gray_bin,
        ),
    );
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(_)) => Err("cannot start gray agent".to_string()),
        Err(_) => Err("gray cmd timed out".to_string()),
    }
}

/// The conversation home exists before gray sees it, same as runner.rs.
fn ensure_home(home: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .map_err(|_| "cannot create conversation home".to_string())?;
    let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bang_and_slash() {
        assert_eq!(
            parse_prefixed("!model anthropic/claude-sonnet-4.6", "UBOT"),
            Some(("model".to_string(), "anthropic/claude-sonnet-4.6".to_string()))
        );
        assert_eq!(
            parse_prefixed("/help", "UBOT"),
            Some(("help".to_string(), String::new()))
        );
        assert_eq!(
            parse_prefixed("  !cron   ", "UBOT"),
            Some(("cron".to_string(), String::new()))
        );
    }

    #[test]
    fn strips_a_leading_mention() {
        assert_eq!(
            parse_prefixed("<@UBOT> !cron", "UBOT"),
            Some(("cron".to_string(), String::new()))
        );
        assert_eq!(
            parse_prefixed("<@UBOT>: /model x", "UBOT"),
            Some(("model".to_string(), "x".to_string()))
        );
        // A mention of somebody else is not the trigger.
        assert_eq!(parse_prefixed("<@UOTHER> !cron", "UBOT"), None);
    }

    #[test]
    fn rejects_non_commands() {
        assert_eq!(parse_prefixed("hello", "UBOT"), None);
        assert_eq!(parse_prefixed("!", "UBOT"), None);
        assert_eq!(parse_prefixed("/", "UBOT"), None);
        assert_eq!(parse_prefixed("!9lives", "UBOT"), None);
        assert_eq!(parse_prefixed("!FOO", "UBOT"), None);
        assert_eq!(parse_prefixed("say !help me", "UBOT"), None);
        assert_eq!(parse_prefixed("/etc/hosts", "UBOT"), None);
        assert_eq!(parse_prefixed("!a.b", "UBOT"), None);
    }

    #[test]
    fn dispatch_verifies_against_the_registry() {
        let names = static_names();
        let model = Cmd {
            scope: Scope::Conversation,
            slow: false,
        };
        assert_eq!(
            dispatch("!model x", "UBOT", &names),
            Some(("model".to_string(), "x".to_string(), model))
        );
        // `!nope` parses but is nobody's command: it stays a prompt.
        assert_eq!(dispatch("!nope", "UBOT", &names), None);
        assert_eq!(dispatch("!status", "UBOT", &names), None);
    }

    #[test]
    fn list_reads_names_scopes_and_aliases() {
        let reply = json!({
            "ok": true,
            "result": {"commands": [
                {"name": "model", "scope": "conversation", "aliases": ["m"]},
                {"name": "gateway", "scope": "global", "aliases": []},
                {"name": "theme", "scope": "global"}
            ]}
        });
        let names = parse_list(&reply).unwrap();
        let conv = Cmd {
            scope: Scope::Conversation,
            slow: false,
        };
        let glob = Cmd {
            scope: Scope::Global,
            slow: false,
        };
        assert_eq!(names.get("model"), Some(&conv));
        assert_eq!(names.get("m"), Some(&conv));
        assert_eq!(names.get("gateway"), Some(&glob));
        assert_eq!(names.get("theme"), Some(&glob));
        assert!(parse_list(&json!({"ok": false})).is_none());
        assert!(parse_list(&json!({"ok": true, "result": {"commands": []}})).is_none());
    }

    #[test]
    fn ui_results_render_their_fallback_text() {
        let reply = json!({
            "ok": true,
            "result": {
                "kind": "ui",
                "token": "abc",
                "document": {"protocol": "gray.ui", "version": 1},
                "fallback_text": "Pick a model:\n1. anthropic/claude-sonnet-4.6\nReply `!model <id>` to choose."
            }
        });
        match result_text(&reply).unwrap() {
            Outcome::Text(text) => assert_eq!(
                text,
                "Pick a model:\n1. anthropic/claude-sonnet-4.6\nReply `!model <id>` to choose."
            ),
            other => panic!("expected text: {other:?}"),
        }
    }

    #[test]
    fn text_results_and_errors() {
        let text = json!({"ok": true, "result": {"kind": "text", "text": "**hi**"}});
        match result_text(&text).unwrap() {
            Outcome::Text(t) => assert_eq!(t, "**hi**"),
            other => panic!("expected text: {other:?}"),
        }
        let err = json!({
            "ok": false,
            "error": {"code": "local_only", "message": "`theme` is TUI-only", "retryable": false}
        });
        assert_eq!(result_text(&err), Err("`theme` is TUI-only".to_string()));
    }
}
