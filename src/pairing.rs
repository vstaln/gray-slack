//! Slack-side pairing. A user who DMs an unconfigured
//! bot is told their own Slack member ID and a one-shot code; the owner
//! approves the code with `gray slack pairing approve slack <code>` and
//! that user joins the allowlist. No terminal wizard, no 5-minute timer,
//! nothing to hunt — the discovery happens where the user already is.

use serde_json::Value;
use std::path::Path;

/// A fresh code: 8 uppercase alphanumerics (short enough
/// to read off a phone, 36^8 space, single-use).
pub fn gen_code() -> String {
    const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 8];
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut bytes);
        }
    }
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// The reply an unconfigured DMer sees. Their own ID comes from Slack
/// itself, so they cannot typo it; the code is single-use.
pub fn unconfigured_reply(user_id: &str, code: &str) -> String {
    format!(
        "access not configured.\n\
         Your Slack member id: `{user_id}`\n\
         Pairing code: `{code}`\n\
         Ask the bot owner to approve with:\n\
         `gray slack pairing approve slack {code}`"
    )
}

/// What the gateway does when a message is not admitted and the sender is a
/// human DMing the bot: mint (or reuse) a code and reply. Bots, channel
/// messages, and admitted senders never see this — `None` for them.
pub struct PairingReply {
    pub text: String,
    #[allow(dead_code)]
    pub code: String,
}

pub fn reply_for(
    store: &crate::store::Store,
    author: &str,
    dm: bool,
    bot: bool,
) -> Option<PairingReply> {
    if bot || !dm || author.is_empty() {
        return None;
    }
    let code = store
        .pairing_code_for(author)
        .ok()
        .flatten()
        .unwrap_or_else(gen_code);
    store.pairing_insert(&code, author).ok()?;
    Some(PairingReply {
        text: unconfigured_reply(author, &code),
        code,
    })
}

/// Consume a code and admit its user: they become `owner_id` if the config
/// has none, otherwise they join `allowed_users`. Config is rewritten
/// privately (0600) exactly as the wizard would.
pub fn approve(config_path: &Path, platform: &str, code: &str) -> Result<String, String> {
    if platform != "slack" {
        return Err(format!(
            "unknown platform '{platform}'; the only one is slack"
        ));
    }
    let store = crate::gateway::open_store(config_path)?;
    let user = store
        .pairing_take(code.trim())?
        .ok_or_else(|| "unknown or already-used pairing code".to_string())?;
    if !crate::config::user_id(&Value::String(user.clone())) {
        return Err("pairing code did not resolve to a Slack member ID".to_string());
    }
    let mut config = crate::config::load_config(config_path)?;
    let owner_missing = config
        .get("owner_id")
        .and_then(Value::as_str)
        .is_none_or(|o| o.is_empty());
    let mut note;
    if owner_missing {
        config["owner_id"] = Value::String(user.clone());
        note = format!("{user} is now the bot owner");
    } else {
        let mut allowed: Vec<String> = config
            .get("allowed_users")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if allowed.iter().any(|id| id == &user) {
            return Ok(format!("{user} was already allowed"));
        }
        allowed.push(user.clone());
        config["allowed_users"] = Value::Array(allowed.into_iter().map(Value::String).collect());
        note = format!("{user} can now trigger the bot");
    }
    crate::config::save_config(config_path, &config)?;
    note.push_str("; restart the gateway to apply (it re-reads on start)");
    Ok(note)
}
