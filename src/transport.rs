//! Slack Web API client: auth.test, apps.connections.open, chat.postMessage,
//! and the files.getUploadURLExternal -> upload -> files.completeUploadExternal
//! v2 flow. Errors never carry tokens.
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const API: &str = "https://slack.com/api";

/// What the transport can report. `Api` is Slack's own `error` string —
/// safe to show. Never includes request secrets.
#[derive(Debug)]
pub enum SlackError {
    /// Slack answered `{"ok":false,"error":..}` or an HTTP status we report.
    Api(String),
    /// Network/client failure (DNS, TLS, timeout, malformed body).
    Transport(String),
}

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(e) => write!(f, "{e}"),
            Self::Transport(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for SlackError {}

fn map(e: reqwest::Error) -> SlackError {
    SlackError::Transport(format!(
        "request failed: {}",
        e.status()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "io".to_string())
    ))
}

#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    bot_token: String,
    app_token: String,
    base: String,
}

/// `auth.test` answer.
#[derive(Debug, Clone)]
pub struct Auth {
    pub user_id: String,
    pub bot_id: String,
    pub team_id: String,
    pub team: String,
    pub user: String,
}

impl Rest {
    /// Real Slack API, 20 s client ceiling.
    pub fn production(bot_token: &str, app_token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Rest {
            http,
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
            base: API.to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base(bot_token: &str, app_token: &str, base: &str) -> Self {
        Rest {
            http: reqwest::Client::new(),
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
            base: base.to_string(),
        }
    }

    /// POST {base}/{method} as JSON with the bot token; `app` picks the
    /// app-level token instead (apps.* methods).
    async fn api(&self, method: &str, body: &Value, app: bool) -> Result<Value, SlackError> {
        let token = if app {
            &self.app_token
        } else {
            &self.bot_token
        };
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(20))
            .json(body)
            .send()
            .await
            .map_err(map)?;
        let status = resp.status();
        let data: Value = resp.json().await.map_err(|_| {
            SlackError::Transport(format!("unreadable {method} response (HTTP {status})"))
        })?;
        if data.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = data
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            return Err(SlackError::Api(format!("{method}: {err}")));
        }
        Ok(data)
    }

    /// POST {base}/{method} as form fields with the bot token.
    async fn api_form(&self, method: &str, fields: &[(&str, String)]) -> Result<Value, SlackError> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .bearer_auth(&self.bot_token)
            .timeout(Duration::from_secs(20))
            .form(fields)
            .send()
            .await
            .map_err(map)?;
        let status = resp.status();
        let data: Value = resp.json().await.map_err(|_| {
            SlackError::Transport(format!("unreadable {method} response (HTTP {status})"))
        })?;
        if data.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = data
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            return Err(SlackError::Api(format!("{method}: {err}")));
        }
        Ok(data)
    }

    /// auth.test — proves the bot token and names the bot.
    pub async fn auth_test(&self) -> Result<Auth, SlackError> {
        let data = self.api("auth.test", &serde_json::json!({}), false).await?;
        let s = |k: &str| {
            data.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Ok(Auth {
            user_id: s("user_id"),
            bot_id: s("bot_id"),
            team_id: s("team_id"),
            team: s("team"),
            user: s("user"),
        })
    }

    /// apps.connections.open — the Socket Mode websocket URL (app token).
    pub async fn connections_open(&self) -> Result<String, SlackError> {
        let data = self
            .api("apps.connections.open", &serde_json::json!({}), true)
            .await?;
        data.get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SlackError::Api("apps.connections.open: no url".to_string()))
    }

    /// One chat.postMessage. `thread_ts` replies inside the thread.
    /// Returns the posted message ts.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String, SlackError> {
        let mut body = serde_json::json!({
            "channel": channel,
            "text": text,
            "unfurl_links": false,
            "unfurl_media": false
        });
        if let Some(ts) = thread_ts {
            body["thread_ts"] = Value::String(ts.to_string());
        }
        let data = self.api("chat.postMessage", &body, false).await?;
        Ok(data
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Post `text` as mrkdwn, chunked at the adapter's limit.
    pub async fn post_text(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<usize, SlackError> {
        let chunks = crate::mrkdwn::chunk(&crate::mrkdwn::convert(text));
        if chunks.is_empty() {
            return Ok(0);
        }
        let mut sent = 0;
        for part in &chunks {
            self.post_message(channel, part, thread_ts).await?;
            sent += 1;
        }
        Ok(sent)
    }

    /// files.upload replacement (v2): getUploadURLExternal -> POST bytes to
    /// the URL -> completeUploadExternal bound to the channel.
    /// Returns the Slack file id.
    pub async fn upload_file(
        &self,
        channel: &str,
        path: &Path,
        title: Option<&str>,
        comment: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<String, SlackError> {
        let bytes =
            std::fs::read(path).map_err(|_| SlackError::Api("cannot read the file".to_string()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let up = self
            .api_form(
                "files.getUploadURLExternal",
                &[
                    ("filename", name.clone()),
                    ("length", bytes.len().to_string()),
                ],
            )
            .await?;
        let upload_url = up
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or_else(|| SlackError::Api("getUploadURLExternal: no url".to_string()))?
            .to_string();
        let file_id = up
            .get("file_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let resp = self
            .http
            .post(&upload_url)
            .timeout(Duration::from_secs(120))
            .body(bytes)
            .send()
            .await
            .map_err(map)?;
        if !resp.status().is_success() {
            return Err(SlackError::Api(format!(
                "file upload rejected (HTTP {})",
                resp.status()
            )));
        }
        let title = title.unwrap_or(&name).to_string();
        let mut body = serde_json::json!({
            "files": [{"id": file_id, "title": title}],
            "channel_id": channel,
        });
        if let Some(comment) = comment {
            body["initial_comment"] = Value::String(comment.to_string());
        }
        if let Some(ts) = thread_ts {
            body["thread_ts"] = Value::String(ts.to_string());
        }
        self.api("files.completeUploadExternal", &body, false)
            .await?;
        Ok(file_id)
    }
}

impl Rest {
    /// conversations.open — the (existing or new) DM channel with a user.
    pub async fn conversations_open(&self, user: &str) -> Result<String, SlackError> {
        let data = self
            .api(
                "conversations.open",
                &serde_json::json!({"users": user}),
                false,
            )
            .await?;
        data.pointer("/channel/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SlackError::Api("conversations.open: no channel".to_string()))
    }
}
