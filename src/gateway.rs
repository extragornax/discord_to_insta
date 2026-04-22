//! Minimal Discord Gateway v10 client. All it does is hold a WebSocket
//! connection open so the bot appears **online** in Discord's user list —
//! we don't subscribe to any events (`intents: 0`) because message ingestion
//! still goes through the REST poller in `main.rs`.
//!
//! Protocol reference: https://discord.com/developers/docs/topics/gateway

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::{Message, protocol::CloseFrame};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const LOG_MAX_LINES: usize = 40;

/// GUILD_MESSAGES (bit 9). Not privileged. Gives us MESSAGE_CREATE /
/// MESSAGE_UPDATE / MESSAGE_DELETE dispatches — we only act on CREATE.
/// We deliberately do NOT request MESSAGE_CONTENT (bit 15, privileged)
/// since the `content` field isn't needed here: we only use the event
/// to fire an early REST fetch.
const IDENTIFY_INTENTS: u64 = 1 << 9;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

pub struct GatewayCtx {
    pub token: String,
    pub channel_id: String,
    pub stop_flag: Arc<AtomicBool>,
    pub log: Arc<Mutex<VecDeque<String>>>,
    pub connected: Arc<AtomicBool>,
    /// Poked on every MESSAGE_CREATE for `channel_id`. The poller awaits this
    /// alongside its timer so new announcements get reacted to in seconds.
    pub poll_trigger: Arc<tokio::sync::Notify>,
    /// Every MESSAGE_UPDATE for `channel_id` pushes the message id here.
    /// The edit-watcher task in `main.rs` consumes these and fetches the
    /// updated body via REST (we don't have MESSAGE_CONTENT intent).
    pub edit_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

pub async fn run(ctx: GatewayCtx) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if ctx.stop_flag.load(Ordering::Relaxed) {
            return;
        }

        push(&ctx.log, "gateway: connecting…").await;
        match connect_once(&ctx).await {
            ConnectOutcome::Disconnected(reason) => {
                ctx.connected.store(false, Ordering::Relaxed);
                push(&ctx.log, &format!("gateway: {reason}")).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            ConnectOutcome::Fatal(reason) => {
                ctx.connected.store(false, Ordering::Relaxed);
                push(
                    &ctx.log,
                    &format!("gateway: fatal, not reconnecting — {reason}"),
                )
                .await;
                return;
            }
            ConnectOutcome::Stopped => {
                ctx.connected.store(false, Ordering::Relaxed);
                push(&ctx.log, "gateway: stopped").await;
                return;
            }
            ConnectOutcome::CleanRestart => {
                // Server asked us to reconnect — no backoff penalty.
                ctx.connected.store(false, Ordering::Relaxed);
                backoff = Duration::from_secs(1);
            }
        }

        // Interruptible backoff.
        let mut elapsed = Duration::ZERO;
        while elapsed < backoff {
            if ctx.stop_flag.load(Ordering::Relaxed) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            elapsed += Duration::from_millis(200);
        }
    }
}

enum ConnectOutcome {
    /// Abnormal termination (network, parse error, etc.). Reconnect with backoff.
    Disconnected(String),
    /// Clean server-requested reconnect (op 7 / 9).
    CleanRestart,
    /// Discord said the config is bad (invalid token, intents, etc.). Don't
    /// reconnect — it'll just fail the same way forever.
    Fatal(String),
    /// Stop flag set.
    Stopped,
}

async fn connect_once(ctx: &GatewayCtx) -> ConnectOutcome {
    let (ws, _) = match connect_async(GATEWAY_URL).await {
        Ok(ok) => ok,
        Err(e) => return ConnectOutcome::Disconnected(format!("dial: {e}")),
    };
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));

    // HELLO (op 10) carries heartbeat_interval.
    let hello = match next_json(&mut read).await {
        Some(v) => v,
        None => return ConnectOutcome::Disconnected("closed before HELLO".into()),
    };
    let hb_ms = match hello["d"]["heartbeat_interval"].as_u64() {
        Some(ms) => ms,
        None => return ConnectOutcome::Disconnected("malformed HELLO".into()),
    };
    let heartbeat_interval = Duration::from_millis(hb_ms);

    // IDENTIFY (op 2) with GUILD_MESSAGES so we get MESSAGE_CREATE events
    // to drive the fast-path poll trigger.
    let identify = json!({
        "op": 2,
        "d": {
            "token": ctx.token,
            "intents": IDENTIFY_INTENTS,
            "properties": {
                "os": "linux",
                "browser": "discord_to_insta",
                "device": "discord_to_insta"
            }
        }
    });
    if let Err(e) = write
        .lock()
        .await
        .send(Message::text(identify.to_string()))
        .await
    {
        return ConnectOutcome::Disconnected(format!("identify: {e}"));
    }

    // Identifying is enough to flip the bot to online in Discord's UI.
    ctx.connected.store(true, Ordering::Relaxed);

    // Heartbeat task.
    let seq: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let hb_stop = ctx.stop_flag.clone();
    let hb_write = write.clone();
    let hb_seq = seq.clone();
    let hb = tokio::spawn(async move {
        loop {
            if interruptible_sleep(&hb_stop, heartbeat_interval).await {
                return;
            }
            let s = *hb_seq.lock().await;
            let payload = json!({"op": 1, "d": s}).to_string();
            if hb_write.lock().await.send(Message::text(payload)).await.is_err() {
                return;
            }
        }
    });

    // Receive loop.
    let outcome = loop {
        if ctx.stop_flag.load(Ordering::Relaxed) {
            break ConnectOutcome::Stopped;
        }
        match read.next().await {
            Some(Ok(msg)) => {
                if msg.is_close() {
                    // 4004 = Authentication failed (bad token). 4010–4014 =
                    // misconfigured shard/intents/api version. All are
                    // operator errors — reconnecting will just loop forever.
                    if let Some(code) = close_code(&msg) {
                        if matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014) {
                            break ConnectOutcome::Fatal(close_reason(&msg));
                        }
                    }
                    break ConnectOutcome::Disconnected(close_reason(&msg));
                }
                let Ok(text) = msg.to_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text) else { continue };

                if let Some(s) = v.get("s").and_then(|x| x.as_u64()) {
                    *seq.lock().await = Some(s);
                }

                match v["op"].as_u64() {
                    Some(0) => {
                        let t = v.get("t").and_then(|t| t.as_str());
                        match t {
                            Some("READY") => {
                                let user = v["d"]["user"]["username"]
                                    .as_str()
                                    .unwrap_or("?");
                                push(&ctx.log, &format!("gateway: READY as {user}")).await;
                            }
                            Some("MESSAGE_CREATE") => {
                                // Only care about our target channel. Fire the
                                // trigger so the poller fetches immediately
                                // instead of waiting up to 30 s.
                                let ch = v["d"]["channel_id"].as_str().unwrap_or("");
                                if ch == ctx.channel_id {
                                    let id = v["d"]["id"].as_str().unwrap_or("?");
                                    push(
                                        &ctx.log,
                                        &format!("gateway: new message {id} — triggering fetch"),
                                    )
                                    .await;
                                    ctx.poll_trigger.notify_one();
                                }
                            }
                            Some("MESSAGE_UPDATE") => {
                                // Discord fires this for content edits, embed
                                // resolves (e.g. a URL in the message gets a
                                // preview embed), pin status changes, etc. We
                                // forward unconditionally and let the edit
                                // watcher in main.rs fetch + diff.
                                let ch = v["d"]["channel_id"].as_str().unwrap_or("");
                                if ch == ctx.channel_id {
                                    let id = v["d"]["id"].as_str().unwrap_or("").to_string();
                                    if !id.is_empty() {
                                        push(
                                            &ctx.log,
                                            &format!("gateway: edit detected on {id}"),
                                        )
                                        .await;
                                        if let Some(tx) = &ctx.edit_tx {
                                            let _ = tx.send(id);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(1) => {
                        // Server-requested immediate heartbeat.
                        let s = *seq.lock().await;
                        let payload = json!({"op": 1, "d": s}).to_string();
                        let _ = write.lock().await.send(Message::text(payload)).await;
                    }
                    Some(7) | Some(9) => {
                        // Reconnect / invalid session. Fresh identify next round.
                        break ConnectOutcome::CleanRestart;
                    }
                    _ => {}
                }
            }
            Some(Err(e)) => {
                break ConnectOutcome::Disconnected(format!("ws: {e}"));
            }
            None => {
                break ConnectOutcome::Disconnected("stream ended".into());
            }
        }
    };

    hb.abort();
    // Best-effort graceful close. We don't care about errors here.
    let _ = close(write).await;
    outcome
}

async fn close(write: Arc<Mutex<WsSink>>) -> tokio_tungstenite::tungstenite::Result<()> {
    write
        .lock()
        .await
        .send(Message::Close(Some(CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
            reason: "bye".into(),
        })))
        .await
}

fn close_reason(msg: &Message) -> String {
    if let Message::Close(Some(frame)) = msg {
        format!("closed: {} {}", frame.code, frame.reason)
    } else {
        "closed".into()
    }
}

fn close_code(msg: &Message) -> Option<u16> {
    if let Message::Close(Some(frame)) = msg {
        Some(u16::from(frame.code))
    } else {
        None
    }
}

async fn next_json<S>(read: &mut S) -> Option<Value>
where
    S: futures_util::Stream<
            Item = Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    loop {
        match read.next().await? {
            Ok(msg) => {
                let Ok(text) = msg.to_text() else { continue };
                if let Ok(v) = serde_json::from_str::<Value>(text) {
                    return Some(v);
                }
            }
            Err(_) => return None,
        }
    }
}

/// Sleep up to `total`, polling the stop flag every 200 ms. Returns `true`
/// when the sleep was cut short by a stop request.
async fn interruptible_sleep(stop_flag: &Arc<AtomicBool>, total: Duration) -> bool {
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if stop_flag.load(Ordering::Relaxed) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        elapsed += Duration::from_millis(200);
    }
    false
}

async fn push(log: &Arc<Mutex<VecDeque<String>>>, line: &str) {
    let mut l = log.lock().await;
    while l.len() >= LOG_MAX_LINES {
        l.pop_front();
    }
    l.push_back(line.to_string());
}
