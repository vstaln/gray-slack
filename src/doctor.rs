//! Doctor preflight checks with safe errors.
//! Every failure is a category string; transport/config bodies never surface.
use serde_json::Value;

use crate::transport::Rest;

/// All checks: gray binary → provider model → bot token → app token →
/// home channel shape.
pub async fn doctor(config: &Value) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    let bot = config
        .get("bot_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let app = config
        .get("app_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let rest = Rest::production(bot, app);
    let auth = rest
        .auth_test()
        .await
        .map_err(|e| format!("bot token failed auth.test: {e}"))?;
    rest.connections_open()
        .await
        .map_err(|e| format!("app token failed apps.connections.open (Socket Mode): {e}"))?;
    let channel = config
        .get("channel_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !crate::config::channel_id(&Value::String(channel.to_string())) {
        return Err("channel_id must be a Slack channel ID".to_string());
    }
    println!(
        "Bot token (@{}), Socket Mode, channel shape and local gray configuration verified.",
        auth.user
    );
    println!("Provider generation and a live message round-trip were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
