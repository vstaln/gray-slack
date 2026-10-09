//! Slack Socket Mode: one WebSocket carrying JSON envelopes we must ack.
//!
//! `apps.connections.open` (app token) hands back a wss URL; every inbound
//! frame is an envelope with an `envelope_id` to echo back within ~3s.
//! `{"type":"disconnect"}` and a dead socket both mean: get a fresh URL and
//! reconnect. Interactive payloads can carry `accepts_response_payload`; v1
//! never replies on the socket — acks only.
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// One inbound envelope (already acked by the reader).
#[derive(Debug)]
pub enum Frame {
    /// `type=events_api`: the Events API payload (`payload.event` inside).
    Event(Value),
    /// `type=interactive` / `slash_commands`: acked, out of scope for v1.
    Ignored,
    /// `type=disconnect` or the socket ended: reconnect.
    Disconnected(String),
}

/// One live socket connection. `next()` yields envelopes until the link
/// drops; the caller reconnects via `Rest::connections_open`.
pub struct Link {
    ws: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
}

impl Link {
    /// Open the socket-mode websocket. `url` comes from
    /// `Rest::connections_open` and is single-use.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(20),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .map_err(|_| "socket mode connect timed out".to_string())?
        .map_err(|_| "socket mode connect failed".to_string())?;
        let (tx, rx) = ws.split();
        Ok(Link { ws: rx, tx })
    }

    async fn ack(&mut self, envelope_id: &str) -> Result<(), String> {
        let body = json!({"envelope_id": envelope_id});
        tokio::time::timeout(
            Duration::from_secs(5),
            self.tx.send(Message::Text(body.to_string().into())),
        )
        .await
        .map_err(|_| "socket ack timed out".to_string())?
        .map_err(|_| "socket ack failed".to_string())
    }

    /// Next inbound frame; `Frame::Disconnected` means the link is over.
    pub async fn next(&mut self) -> Frame {
        loop {
            let msg = match self.ws.next().await {
                Some(Ok(m)) => m,
                Some(Err(_)) => return Frame::Disconnected("socket error".to_string()),
                None => return Frame::Disconnected("socket closed".to_string()),
            };
            match msg {
                Message::Ping(data) => {
                    let _ = self.tx.send(Message::Pong(data)).await;
                    continue;
                }
                Message::Close(_) => return Frame::Disconnected("close frame".to_string()),
                Message::Text(raw) => {
                    let env: Value = match serde_json::from_str(&raw) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(id) = env.get("envelope_id").and_then(Value::as_str) {
                        if self.ack(id).await.is_err() {
                            return Frame::Disconnected("ack failed".to_string());
                        }
                    }
                    match env.get("type").and_then(Value::as_str) {
                        Some("events_api") => {
                            let event = env
                                .get("payload")
                                .and_then(|p| p.get("event"))
                                .cloned()
                                .unwrap_or(Value::Null);
                            return Frame::Event(event);
                        }
                        Some("disconnect") => {
                            let reason = env
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            return Frame::Disconnected(reason);
                        }
                        // hello, slash_commands, interactive: seen and acked.
                        _ => return Frame::Ignored,
                    }
                }
                _ => continue,
            }
        }
    }
}
