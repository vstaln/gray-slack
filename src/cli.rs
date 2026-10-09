//! CLI tree: `gray-slack --config <path> <subcommand>`.
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Standalone Slack transport and setup for gray.
#[derive(Debug, Parser)]
#[command(name = "gray-slack", bin_name = "gray slack")]
pub struct Cli {
    /// Config file path.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn config_path(&self) -> PathBuf {
        match &self.config {
            Some(p) => p.clone(),
            None => crate::config::default_path(),
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Interactive setup: both tokens checked with Slack, owner and
    /// allowlist collected, home channel is your DM with the bot.
    Setup,
    Run,
    Sidecar,
    /// Print the plugin manifest (what `gray plugin install slack` reads).
    #[command(hide = true)]
    Manifest,
    Register,
    Install,
    Status,
    Stop,
    Restart,
    Doctor,
    Uninstall,
    Limits {
        #[arg(long)]
        timeout_seconds: Option<u64>,
        #[arg(long)]
        concurrency: Option<u64>,
        #[arg(long)]
        max_requests: Option<u64>,
    },
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    Allowlist {
        #[command(subcommand)]
        action: AllowlistAction,
    },
    Pairing {
        #[command(subcommand)]
        action: PairingAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum QueueAction {
    List,
    Cancel { id: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum AllowlistAction {
    Add { id: String },
    Remove { id: String },
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PairingAction {
    /// Approve a code the bot handed an unconfigured DMer: they join the
    /// allowlist (or become the owner if none is set yet).
    Approve {
        /// Platform the code was issued for. Only `slack` exists.
        platform: String,
        /// The code from the bot's DM reply.
        code: String,
    },
}

/// Register the outgoing `slack_*` tools in gray's plugin lock.
/// Matches `gray_plugin::lock::LockEntry` (explicit argv, no fake install).
pub fn register(config: &Value, config_path: &Path) -> Result<(), String> {
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let lock = Path::new(gray_home).join("plugins/lock.json");
    let mut data: Value = std::fs::read(&lock)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({"schema": 1, "plugins": {}}));
    if data.get("schema").and_then(Value::as_u64) != Some(1)
        || !data.get("plugins").is_some_and(Value::is_object)
    {
        return Err("Unsupported gray plugin lock; not changing it".to_string());
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|_| "cannot locate plugin binary".to_string())?;
    let resolved = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    let argv = vec![
        Value::String(exe.clone()),
        Value::String("sidecar".to_string()),
        Value::String("--config".to_string()),
        Value::String(resolved),
    ];
    let prev_exe = data
        .pointer("/plugins/slack/argv/0")
        .and_then(Value::as_str);
    if prev_exe.is_some_and(|p| p != exe && Path::new(p).is_file()) {
        return Err("Another slack plugin is registered; refusing to overwrite it".to_string());
    }
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    // Merge into any existing row: gray keeps cli_argv and the consent
    // record in the same lock entry, so replacing the object would drop them.
    let entry = &mut data["plugins"]["slack"];
    if !entry.is_object() {
        *entry = json!({});
    }
    entry["ecosystem"] = json!("gray-native");
    entry["version"] = json!(env!("CARGO_PKG_VERSION"));
    entry["source"] = json!("https://github.com/vstaln/gray-slack");
    entry["argv"] = Value::Array(argv);
    entry["adapter_version"] = json!("1.1");
    entry["installed_at"] = json!(installed_at);
    if entry.get("hash").is_none() {
        entry["hash"] = json!("");
    }
    if entry.get("scope").is_none() {
        entry["scope"] = json!("user");
    }
    if entry.get("enabled").is_none() {
        entry["enabled"] = json!(true);
    }
    // Re-register heals a row whose CLI binary moved (reinstall, cargo
    // install to a new prefix); a working custom cli_argv is kept.
    let cli_ok = entry
        .get("cli_argv")
        .and_then(|a| a.get(0))
        .and_then(Value::as_str)
        .is_some_and(|p| Path::new(p).is_file());
    if !cli_ok {
        entry["cli_argv"] = json!([exe]);
    }
    crate::config::atomic_json(&lock, &data)
}

/// Parse the service-install answer: default yes.
pub fn install_confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
}

fn limits(
    config_path: &Path,
    t: Option<u64>,
    c: Option<u64>,
    m: Option<u64>,
) -> Result<(), String> {
    let mut config = crate::config::load_config(config_path)?;
    if let Some(v) = t {
        config["timeout_seconds"] = json!(v);
    }
    if let Some(v) = c {
        config["concurrency"] = json!(v);
    }
    if let Some(v) = m {
        config["max_requests"] = json!(v);
    }
    crate::config::save_config(config_path, &config)?;
    println!("Limits saved; restart the gateway to apply.");
    Ok(())
}

fn queue(action: &QueueAction, config_path: &Path) -> Result<(), String> {
    let store = crate::gateway::open_store(config_path)?;
    match action {
        QueueAction::List => {
            let items = store.items()?;
            if items.is_empty() {
                println!("queue empty");
            }
            for i in items {
                let text: String = i.text.chars().take(60).collect();
                println!("{} [{}] {} ({}…)", i.id, i.state, i.channel, text);
            }
            Ok(())
        }
        QueueAction::Cancel { id } => {
            if store.cancel(id)? {
                println!("cancelled {id}");
                Ok(())
            } else {
                Err(format!("{id}: not queued (running or unknown)"))
            }
        }
    }
}

fn allowlist(action: &AllowlistAction, config_path: &Path) -> Result<(), String> {
    let mut config = crate::config::load_config(config_path)?;
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
    match action {
        AllowlistAction::Add { id } => {
            if !crate::config::user_id(&Value::String(id.clone())) {
                return Err("member IDs look like U0123ABCDEF".to_string());
            }
            if !allowed.contains(id) {
                allowed.push(id.clone());
            }
            config["allowed_users"] =
                Value::Array(allowed.into_iter().map(Value::String).collect());
            crate::config::save_config(config_path, &config)?;
            println!("{id} allowed; restart the gateway to apply.");
            Ok(())
        }
        AllowlistAction::Remove { id } => {
            allowed.retain(|u| u != id);
            config["allowed_users"] =
                Value::Array(allowed.into_iter().map(Value::String).collect());
            crate::config::save_config(config_path, &config)?;
            println!("{id} removed; restart the gateway to apply.");
            Ok(())
        }
        AllowlistAction::List => {
            let owner = config.get("owner_id").and_then(Value::as_str).unwrap_or("");
            println!("owner: {}", if owner.is_empty() { "(none)" } else { owner });
            for u in &allowed {
                println!("{u}");
            }
            Ok(())
        }
    }
}

/// Dispatch a parsed subcommand.
pub fn run(cmd: &Command, config_path: &Path) -> Result<(), String> {
    match cmd {
        Command::Sidecar => crate::sidecar::serve(config_path),
        Command::Manifest => {
            use clap::CommandFactory;
            let mut m = crate::sidecar::MANIFEST.clone();
            m["completion"] = Cli::command()
                .get_subcommands()
                .filter(|c| {
                    !c.is_hide_set() && !["run", "sidecar", "register"].contains(&c.get_name())
                })
                .map(|c| Value::String(c.get_name().to_string()))
                .collect();
            m["sidecar_args"] = serde_json::json!(["sidecar"]);
            println!("{m}");
            Ok(())
        }
        Command::Register => {
            let config: Value = crate::config::load_config(config_path)?;
            register(&config, config_path)
        }
        Command::Setup => {
            use crate::setup::Prompter;
            let mut io = crate::setup::Tty;
            // setup is async; drive it on a throwaway current-thread runtime
            // so `run` stays sync like the other CLI arms.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start setup runtime".to_string())?;
            let done = rt.block_on(crate::setup::run(config_path, &mut io))?;
            if done {
                let config: Value = crate::config::load_config(config_path)?;
                register(&config, config_path)?;
                println!("Outgoing tools registered with gray.");
                if crate::service::installed(config_path) {
                    // A reconfigured bot takes effect now.
                    println!("{}", crate::service::restart(config_path)?);
                    return Ok(());
                }
                let answer = io.prompt("Enable and start the background service now? [Y/n] ")?;
                if install_confirmed(&answer) {
                    let line = crate::service::install(config_path)?;
                    println!("{line}");
                    println!("Run gray slack status to check it.");
                    if matches!(
                        crate::service::detect(),
                        crate::service::Supervisor::SystemdUser { .. }
                    ) {
                        println!("For operation after logout: loginctl enable-linger \"$USER\"");
                    }
                } else {
                    println!(
                        "Run gray slack install when ready, or gray slack run in the foreground."
                    );
                }
            }
            Ok(())
        }
        Command::Run => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start gateway runtime".to_string())?;
            rt.block_on(crate::gateway::run(config_path))
        }
        Command::Status => {
            println!("{}", crate::service::status(config_path)?);
            Ok(())
        }
        Command::Stop => {
            println!("{}", crate::service::stop(config_path)?);
            Ok(())
        }
        Command::Restart => {
            println!("{}", crate::service::restart(config_path)?);
            Ok(())
        }
        Command::Uninstall => {
            let line = crate::service::uninstall(config_path)?;
            println!("{line}");
            println!("Service removed. Private configuration and sessions retained; registered outgoing tools retained.");
            Ok(())
        }
        Command::Install => {
            let line = crate::service::install(config_path)?;
            println!("{line}");
            if matches!(
                crate::service::detect(),
                crate::service::Supervisor::SystemdUser { .. }
            ) {
                println!("To survive logout, enable user linger: loginctl enable-linger \"$USER\"");
            }
            Ok(())
        }
        Command::Doctor => {
            let config = crate::config::load_config(config_path)?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start runtime".to_string())?;
            rt.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    crate::doctor::doctor(&config),
                )
                .await
                .map_err(|_| "doctor timed out after 30s".to_string())?
            })
        }
        Command::Limits {
            timeout_seconds,
            concurrency,
            max_requests,
        } => limits(config_path, *timeout_seconds, *concurrency, *max_requests),
        Command::Queue { action } => queue(action, config_path),
        Command::Allowlist { action } => allowlist(action, config_path),
        Command::Pairing { action } => match action {
            PairingAction::Approve { platform, code } => {
                println!("{}", crate::pairing::approve(config_path, platform, code)?);
                Ok(())
            }
        },
    }
}
