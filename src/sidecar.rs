//! NDJSON protocol 1.1 sidecar.
//! Manifest answers without config; delivery failures never echo secrets.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Static manifest (`slack` 0.1.x, protocol 1.1, no commands).
pub static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "name": "slack",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "requires_plugins": ["gateway"],
        "commands": [],
        "hooks": ["prompt/context"],
        "tools": [{
            "name": "slack_send",
            "label": "Slack Send",
            "description": "Send text to the configured Slack channel (markdown is converted to mrkdwn; long text is chunked). Put MEDIA:/absolute/path on its own line to upload files with it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "Message text (markdown; posted as Slack mrkdwn)"},
                    "channel": {"type": "string", "description": "Slack channel ID; default: the configured home channel"},
                    "thread_ts": {"type": "string", "description": "Reply inside this thread"}
                },
                "required": ["content"]
            }
        }, {
            "name": "slack_file",
            "label": "Slack File",
            "description": "Upload local files to Slack (files.upload v2). Any readable path works except credentials/config files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (absolute, ~/..., or relative to the working directory)"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Up to 10 files, one upload each"},
                    "comment": {"type": "string", "description": "Text posted with the first upload"},
                    "channel": {"type": "string", "description": "Slack channel ID; default: the configured home channel"},
                    "thread_ts": {"type": "string", "description": "Upload inside this thread"}
                }
            }
        }]
    })
});

const PROMPT_CONTEXT: &str = "Slack tools: slack_send posts text to the configured Slack channel (markdown becomes mrkdwn, long text is chunked); add a line MEDIA:/absolute/path to attach files. slack_file uploads local files with an optional comment; pass channel/thread_ts to aim elsewhere. Errors say exactly what to fix; nothing is sent when a call is invalid. Never send secrets.";

fn is_error() -> Value {
    json!({
        "content": "Slack delivery failed. Run gray-slack doctor; do not blindly retry partial sends.",
        "is_error": true
    })
}

/// A specific, fixable failure. `detail` must never contain config values.
fn not_sent(detail: impl std::fmt::Display) -> Value {
    json!({
        "content": format!("Not sent: {detail}. Nothing reached Slack; fix it and call again."),
        "is_error": true
    })
}

/// Failure after Slack was contacted; the detail is Slack's own reason.
fn rejected(error: &crate::transport::SlackError) -> Value {
    json!({
        "content": format!("Not sent: {error}. Nothing was posted; fix the call and try again."),
        "is_error": true
    })
}

/// Dispatch one protocol method. Never echoes config/token text: failures
/// describe the caller's own input or Slack's validation response only.
pub async fn dispatch(method: &str, params: &Value, config_path: &Path) -> Value {
    if method == "plugin/manifest" {
        return MANIFEST.clone();
    }
    if method == "prompt/context" {
        return json!({"text": PROMPT_CONTEXT});
    }
    if method == "tool/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        return match name {
            "slack_send" => send_text(params, config_path).await,
            "slack_file" => file_tool(params, config_path).await,
            _ => is_error(),
        };
    }
    json!({"error": "Unsupported method"})
}

/// The calling session's working directory, falling back to the sidecar's.
fn session_cwd(params: &Value) -> PathBuf {
    params
        .pointer("/session/cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(std::env::temp_dir)
}

/// Tokens + default channel from config; `None` means the bridge is not
/// set up.
fn delivery_target(config_path: &Path) -> Result<(crate::transport::Rest, String), Value> {
    let config = crate::config::load_config(config_path).map_err(|_| is_error())?;
    let bot = config
        .get("bot_token")
        .and_then(Value::as_str)
        .ok_or_else(is_error)?
        .to_string();
    let app = config
        .get("app_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let channel = config
        .get("channel_id")
        .and_then(Value::as_str)
        .ok_or_else(is_error)?
        .to_string();
    Ok((crate::transport::Rest::production(&bot, &app), channel))
}

/// Resolve a user-supplied path: absolute, `~/`, or relative to `cwd`.
/// Refuses the plugin's own config directory (tokens live there).
fn resolve_path(raw: &str, cwd: &Path, config_path: &Path) -> Result<PathBuf, Value> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    let resolved = expanded.canonicalize().unwrap_or(expanded);
    let config_dir = config_path
        .parent()
        .and_then(|d| d.canonicalize().ok())
        .unwrap_or_default();
    if !config_dir.as_os_str().is_empty() && resolved.starts_with(&config_dir) {
        return Err(not_sent("refusing to upload a plugin config file"));
    }
    if !resolved.is_file() {
        return Err(not_sent(format!("{raw}: not a readable file")));
    }
    Ok(resolved)
}

/// `MEDIA:<path>` lines in content, returned with the prose that remains.
fn extract_media(
    content: &str,
    cwd: &Path,
    config_path: &Path,
) -> Result<(String, Vec<PathBuf>), Value> {
    let mut prose = String::new();
    let mut media = Vec::new();
    for line in content.split('\n') {
        if let Some(raw) = line.trim().strip_prefix("MEDIA:") {
            media.push(resolve_path(raw.trim(), cwd, config_path)?);
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    if media.len() > 10 {
        return Err(not_sent("at most 10 MEDIA: files per message"));
    }
    Ok((prose.trim_end().to_string(), media))
}

async fn send_text(params: &Value, config_path: &Path) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    if content.trim().is_empty() {
        return not_sent("content is empty");
    }
    let (rest, home) = match delivery_target(config_path) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let channel = args
        .get("channel")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .unwrap_or(&home)
        .to_string();
    let thread = args.get("thread_ts").and_then(Value::as_str);
    let (prose, media) = match extract_media(content, &session_cwd(params), config_path) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !prose.is_empty() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            rest.post_text(&channel, &prose, thread),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return rejected(&e),
            Err(_) => return is_error(),
        }
    }
    let mut sent = 0;
    for path in &media {
        match rest.upload_file(&channel, path, None, None, thread).await {
            Ok(_) => sent += 1,
            Err(e) => {
                if sent > 0 {
                    return json!({
                        "content": format!("Partially sent: {sent} file(s) reached Slack before a failure; do not resend those."),
                        "is_error": true
                    });
                }
                return rejected(&e);
            }
        }
    }
    let files = if media.is_empty() {
        String::new()
    } else {
        format!(" with {} file(s)", media.len())
    };
    json!({"content": format!("Sent to Slack channel {channel}{files}.")})
}

async fn file_tool(params: &Value, config_path: &Path) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let mut raws: Vec<String> = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(one) = args.get("path").and_then(Value::as_str) {
        raws.insert(0, one.to_string());
    }
    raws.dedup();
    if raws.is_empty() {
        return not_sent("missing `path` (or `paths`)");
    }
    if raws.len() > 10 {
        return not_sent("at most 10 files per call");
    }
    let (rest, home) = match delivery_target(config_path) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let channel = args
        .get("channel")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .unwrap_or(&home)
        .to_string();
    let thread = args.get("thread_ts").and_then(Value::as_str);
    let comment = args.get("comment").and_then(Value::as_str);
    let cwd = session_cwd(params);
    let mut sent = 0;
    for (i, raw) in raws.iter().enumerate() {
        let path = match resolve_path(raw, &cwd, config_path) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let note = if i == 0 { comment } else { None };
        match tokio::time::timeout(
            std::time::Duration::from_secs(180),
            rest.upload_file(&channel, &path, None, note, thread),
        )
        .await
        {
            Ok(Ok(_)) => sent += 1,
            Ok(Err(e)) => {
                if sent > 0 {
                    return json!({
                        "content": format!("Partially sent: {sent} file(s) reached Slack before a failure; do not resend those."),
                        "is_error": true
                    });
                }
                return rejected(&e);
            }
            Err(_) => return is_error(),
        }
    }
    json!({"content": format!("Uploaded {sent} file(s) to Slack channel {channel}.")})
}

/// The sidecar loop: one JSON request per stdin line, one `{"id", "result"}`
/// reply per line on stdout. `plugin/shutdown` and EOF exit.
pub fn serve(config_path: &Path) -> ! {
    use std::io::{BufRead, Write};
    const LIMIT: usize = 256 * 1024;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("sidecar runtime builds");
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => std::process::exit(1),
        };
        if n == 0 {
            std::process::exit(0);
        }
        if buf.len() > LIMIT {
            std::process::exit(1);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !req.is_object() {
            continue;
        }
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            std::process::exit(0);
        }
        let id = match req.get("id") {
            Some(Value::Number(_)) => req.get("id").cloned().unwrap(),
            _ => continue,
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = match req.get("params") {
            Some(Value::Object(_)) => req.get("params").cloned().unwrap(),
            _ => json!({}),
        };
        let result = rt.block_on(dispatch(method, &params, config_path));
        let row = json!({"id": id, "result": result});
        if writeln!(out, "{row}").is_err() {
            std::process::exit(1);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}
