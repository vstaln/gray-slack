//! The Socket Mode daemon: envelopes -> durable inbox -> isolated gray
//! turns -> mrkdwn replies (in the thread when `reply_in_thread`).
//!
//! Admission mirrors the Discord bridge: bots and ourselves never turn;
//! the sender must be the owner or on `allowed_users`; the channel must be
//! a DM, the home channel, or on `allowed_channels`. An un-admitted human
//! DMing the bot gets a pairing reply instead of silence.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::runner::RunError;
use crate::socket::{Frame, Link};
use crate::store::Store;
use crate::transport::Rest;

/// The queue file next to the config.
pub fn open_store(config_path: &Path) -> Result<Store, String> {
    Store::new(&Store::default_path(config_path))
}

fn cfg_str<'a>(config: &'a Value, key: &str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or("")
}

fn cfg_list(config: &Value, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

struct Daemon {
    config: Value,
    config_path: PathBuf,
    store: Store,
    rest: Rest,
    bot_user_id: String,
    commands: crate::commands::Cache,
}

impl Daemon {
    fn home_channel(&self) -> &str {
        cfg_str(&self.config, "channel_id")
    }

    fn reply_in_thread(&self) -> bool {
        self.config
            .get("reply_in_thread")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    fn user_admitted(&self, user: &str) -> bool {
        let owner = cfg_str(&self.config, "owner_id");
        !user.is_empty()
            && (user == owner
                || cfg_list(&self.config, "allowed_users")
                    .iter()
                    .any(|u| u == user))
    }

    fn channel_admitted(&self, channel: &str, is_dm: bool) -> bool {
        is_dm
            || channel == self.home_channel()
            || cfg_list(&self.config, "allowed_channels")
                .iter()
                .any(|c| c == channel)
    }

    /// One Events API event; anything that is not a human message is noise.
    async fn on_event(&self, event: &Value) {
        if event.get("type").and_then(Value::as_str) != Some("message") {
            return;
        }
        // Edits, joins, bot echoes: only real posts (and file uploads) turn.
        let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
        if !matches!(subtype, "" | "file_share") {
            return;
        }
        let channel = event.get("channel").and_then(Value::as_str).unwrap_or("");
        let user = event.get("user").and_then(Value::as_str).unwrap_or("");
        let ts = event.get("ts").and_then(Value::as_str).unwrap_or("");
        if channel.is_empty() || user.is_empty() || ts.is_empty() {
            return;
        }
        let is_bot = event.get("bot_id").is_some() || user == self.bot_user_id;
        let is_dm = channel.starts_with('D');
        if is_bot {
            return;
        }

        let mut text = event
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Strip a direct @bot mention the way the Discord bridge does.
        if !self.bot_user_id.is_empty() {
            let mention = format!("<@{}>", self.bot_user_id);
            text = text.replace(&mention, "");
        }
        text = text.trim().to_string();
        // Attached files become markers the model can read.
        if let Some(files) = event.get("files").and_then(Value::as_array) {
            for f in files {
                if let Some(name) = f.get("name").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("[attached file: {name}]"));
                }
            }
        }
        if text.is_empty() {
            return;
        }
        eprintln!("[slack] message from {user} in {channel} (dm: {is_dm})");

        if !self.user_admitted(user) || !self.channel_admitted(channel, is_dm) {
            // An un-admitted human DMing the bot hears how to get in.
            if let Some(reply) = crate::pairing::reply_for(&self.store, user, is_dm, is_bot) {
                let _ = self.rest.post_text(channel, &reply.text, Some(ts)).await;
            }
            return;
        }

        let thread_root = event
            .get("thread_ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Bare text commands first — they never reach the model.
        if self
            .command(channel, ts, user, &text, is_dm, &thread_root)
            .await
        {
            return;
        }

        let conversation = crate::session::conversation_key(channel, &thread_root, user, is_dm);
        let capacity = self
            .config
            .get("queue_capacity")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        // The reply target: the inbound thread, or the message itself when
        // replies should open a thread.
        let deliver_thread = if self.reply_in_thread() {
            if thread_root.is_empty() {
                ts.to_string()
            } else {
                thread_root.clone()
            }
        } else {
            String::new()
        };
        match self.store.enqueue(
            &format!("{channel}:{ts}"),
            channel,
            &deliver_thread,
            user,
            &conversation,
            &text,
            capacity,
        ) {
            Ok(Some(id)) => eprintln!("[slack] queued {id}"),
            Ok(None) => {} // socket-mode redelivery; already accepted
            Err(e) => {
                let _ = self
                    .rest
                    .post_text(
                        channel,
                        &format!("Queue full ({e}); try again later."),
                        Some(ts),
                    )
                    .await;
            }
        }
    }

    /// `!name args` / `/name args` run the headless `gray cmd` surface;
    /// `/new` and `/reset` keep their local reset. Returns true when the
    /// text was a command — commands answer here, never a queued turn.
    async fn command(
        &self,
        channel: &str,
        ts: &str,
        user: &str,
        text: &str,
        is_dm: bool,
        thread_ts: &str,
    ) -> bool {
        let Some((name, args)) = crate::commands::parse_prefixed(text, &self.bot_user_id)
        else {
            return false;
        };
        let conversation = crate::session::conversation_key(channel, thread_ts, user, is_dm);
        let home = crate::session::conversation_home(&self.config_path, &conversation);
        // `/new` and `/reset` already reset the private home locally; the
        // `!` twins take the same path rather than `gray cmd run new`.
        if matches!(name.as_str(), "new" | "reset") {
            match crate::session::reset_home(&home, crate::store::now_secs()) {
                Ok(()) => {
                    let _ = self
                        .rest
                        .post_text(
                            channel,
                            "Session reset — the next message starts fresh.",
                            Some(ts),
                        )
                        .await;
                }
                Err(_) => {
                    let _ = self
                        .rest
                        .post_text(channel, "Could not reset this session.", Some(ts))
                        .await;
                }
            }
            return true;
        }
        let gray_bin = cfg_str(&self.config, "gray_bin");
        if gray_bin.is_empty() {
            return false;
        }
        let names = self.commands.names(Path::new(gray_bin), &home).await;
        // Unknown name: leave the text for the normal turn.
        let Some(&scope) = names.get(name.as_str()) else {
            return false;
        };
        let cmd_home =
            crate::commands::command_home(&self.config, &self.config_path, &conversation, scope);
        let posted =
            match crate::commands::run(Path::new(gray_bin), &cmd_home, &name, &args).await {
                Ok(text) => text,
                Err(message) => format!("_{message}_"),
            };
        let _ = self.rest.post_text(channel, &posted, Some(ts)).await;
        true
    }

    /// One claimed inbox item end to end.
    async fn work(&self) -> Result<bool, String> {
        let Some(item) = self.store.claim()? else {
            return Ok(false);
        };
        let thread = if item.thread_ts.is_empty() {
            None
        } else {
            Some(item.thread_ts.as_str())
        };
        let outcome = crate::runner::run_gray(
            &self.config,
            &self.config_path,
            &item.conversation,
            &item.text,
            None,
        )
        .await;
        match outcome {
            Ok(answer) => {
                let posted = self.rest.post_text(&item.channel, &answer, thread).await;
                match posted {
                    Ok(_) => self.store.complete(&item.id, &answer)?,
                    Err(e) => {
                        self.store
                            .fail(&item.id, &format!("delivery failed: {e}"))?;
                        eprintln!("[slack] delivery for {} failed: {e}", item.id);
                    }
                }
            }
            Err(RunError::Busy) => {
                self.store.requeue(&item.id)?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                let _ = self
                    .rest
                    .post_text(&item.channel, &format!("_{e}_"), thread)
                    .await;
                self.store.fail(&item.id, &e.to_string())?;
            }
        }
        Ok(true)
    }
}

/// The daemon: boot notices, socket loop, workers, graceful stop.
pub async fn run(config_path: &Path) -> Result<(), String> {
    let config = crate::config::load_config(config_path)?;
    let dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let store = open_store(config_path)?;
    let interrupted = store.recover().unwrap_or(0);

    let rest = Rest::production(cfg_str(&config, "bot_token"), cfg_str(&config, "app_token"));
    let auth = rest
        .auth_test()
        .await
        .map_err(|e| format!("bot token failed auth.test: {e}"))?;
    eprintln!(
        "[slack] connected as @{} ({}) in {}",
        auth.user, auth.user_id, auth.team
    );

    let daemon = Arc::new(Daemon {
        config: config.clone(),
        config_path: config_path.to_path_buf(),
        store,
        rest,
        bot_user_id: auth.user_id.clone(),
        commands: crate::commands::Cache::new(),
    });

    // Startup notice (gray core words it when it can).
    if crate::lifecycle::enabled(&config) {
        let gray_bin = Path::new(cfg_str(&daemon.config, "gray_bin"));
        if let Some(notice) = crate::lifecycle::boot_via(gray_bin, &dir, interrupted).await {
            let _ = daemon
                .rest
                .post_text(daemon.home_channel(), &notice, None)
                .await;
        }
    } else {
        let _ = crate::lifecycle::boot(&dir);
    }

    // Workers drain the inbox; the socket loop only ever enqueues.
    let workers = daemon
        .config
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 16);
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut handles = Vec::new();
    for _ in 0..workers {
        let d = daemon.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match d.work().await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => eprintln!("[slack] worker error: {e}"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    _ = stop.notified() => break,
                }
            }
        }));
    }

    // SIGTERM/SIGINT end the loop; the supervisor's kill window covers
    // the notices below.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            shutdown.notify_waiters();
        });
    }

    // Reconnect loop: a fresh URL per link, bounded backoff.
    let mut backoff = Duration::from_secs(1);
    'outer: loop {
        let mut link = match daemon.rest.connections_open().await {
            Ok(url) => match Link::connect(&url).await {
                Ok(l) => {
                    backoff = Duration::from_secs(1);
                    l
                }
                Err(e) => {
                    eprintln!("[slack] socket connect failed: {e}");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {},
                        _ = shutdown.notified() => break 'outer,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue 'outer;
                }
            },
            Err(e) => {
                eprintln!("[slack] connections.open failed: {e}");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = shutdown.notified() => break 'outer,
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue 'outer;
            }
        };
        eprintln!("[slack] socket mode link up");
        loop {
            tokio::select! {
                frame = link.next() => {
                    match frame {
                        Frame::Event(event) => daemon.on_event(&event).await,
                        Frame::Ignored => {}
                        Frame::Disconnected(reason) => {
                            eprintln!("[slack] socket down ({reason}); reconnecting");
                            break;
                        }
                    }
                }
                _ = shutdown.notified() => break 'outer,
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {},
            _ = shutdown.notified() => break 'outer,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }

    // Graceful stop: tell running chats and the home channel, bounded.
    eprintln!("[slack] shutting down");
    stop.notify_waiters();
    let gray_bin = Path::new(cfg_str(&daemon.config, "gray_bin"));
    let goodbye = crate::lifecycle::stop_via(gray_bin, &dir).await;
    if crate::lifecycle::enabled(&daemon.config) {
        let running: Vec<(String, String)> = daemon
            .store
            .running_channels()
            .unwrap_or_default()
            .into_iter()
            .map(|i| (i.channel, i.thread_ts))
            .collect();
        let targets = crate::lifecycle::shutdown_targets(&running, daemon.home_channel(), &goodbye);
        let notice = async {
            for (channel, thread, text) in targets {
                let ts = if thread.is_empty() {
                    None
                } else {
                    Some(thread.as_str())
                };
                let _ = daemon.rest.post_text(&channel, text, ts).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(4), notice).await;
    }
    for h in handles {
        h.abort();
    }
    Ok(())
}
