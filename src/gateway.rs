//! Minimal Discord Gateway v10 client. Holds a WebSocket open so the bot
//! shows online, fires a Notify on every MESSAGE_CREATE in the target
//! channel so the poller can react in seconds, and — when a `Db` is
//! attached — logs every MESSAGE_CREATE / MESSAGE_UPDATE / MESSAGE_DELETE
//! the bot can see (any channel/guild) to Postgres, mirroring the schema
//! in `../rust-discord-logger`.
//!
//! Intents: GUILD_MESSAGES is always requested. If a `Db` is attached,
//! MESSAGE_CONTENT (privileged) is added so we can store the actual
//! message body. Without DB logging, content isn't needed.
//!
//! Protocol reference: https://discord.com/developers/docs/topics/gateway

use chrono::{DateTime, Utc};
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

use crate::db::Db;
use crate::discord;
use crate::moderation;
use crate::weekly_poll;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const LOG_MAX_LINES: usize = 40;

/// `GUILD_MESSAGES` (bit 9). Not privileged. Gives us MESSAGE_CREATE /
/// MESSAGE_UPDATE / MESSAGE_DELETE dispatches.
const INTENT_GUILD_MESSAGES: u64 = 1 << 9;
/// `MESSAGE_CONTENT` (bit 15). **Privileged** — must be enabled in the
/// Discord Developer Portal. Without it, `content` arrives empty for
/// messages the bot didn't author or wasn't @-mentioned in.
const INTENT_MESSAGE_CONTENT: u64 = 1 << 15;

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
    /// updated body via REST.
    pub edit_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Optional database sink. When `Some`, every visible MESSAGE_CREATE /
    /// MESSAGE_UPDATE / MESSAGE_DELETE is persisted. When `None`, no DB
    /// writes happen and the MESSAGE_CONTENT intent is not requested.
    pub db: Option<Db>,
    /// Used to resolve a `guild_id` to a `guild_name` the first time we
    /// log a message from a new guild. Optional so the gateway can run
    /// without DB logging (the discord::Client is created in main.rs
    /// regardless, so in practice this is always `Some` when token is set).
    pub discord: Option<Arc<discord::Client>>,
    /// Invite-link moderation. When `Some`, every MESSAGE_CREATE is checked
    /// for Discord invite links (warn / escalate / delete).
    pub moderation: Option<Arc<moderation::Moderator>>,
    /// Weekly-poll manual trigger. When `Some`, the `/poll` guild command
    /// is registered on READY and INTERACTION_CREATE events fire it.
    pub weekly_poll_trigger: Option<weekly_poll::ManualTrigger>,
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

    // IDENTIFY (op 2). Add MESSAGE_CONTENT only when DB logging is on so
    // operators without DB don't need to flip the privileged toggle.
    let mut intents = INTENT_GUILD_MESSAGES;
    if ctx.db.is_some() {
        intents |= INTENT_MESSAGE_CONTENT;
    }
    let identify = json!({
        "op": 2,
        "d": {
            "token": ctx.token,
            "intents": intents,
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
                                if let (Some(trigger), Some(client)) =
                                    (&ctx.weekly_poll_trigger, &ctx.discord)
                                {
                                    let app_id =
                                        v["d"]["application"]["id"].as_str().unwrap_or("");
                                    weekly_poll::register_command(
                                        trigger, client, app_id, &ctx.log,
                                    );
                                }
                            }
                            Some("MESSAGE_CREATE") => {
                                handle_message_create(ctx, &v["d"]).await;
                            }
                            Some("MESSAGE_UPDATE") => {
                                handle_message_update(ctx, &v["d"]).await;
                            }
                            Some("MESSAGE_DELETE") => {
                                handle_message_delete(ctx, &v["d"]).await;
                            }
                            Some("INTERACTION_CREATE") => {
                                if let Some(trigger) = &ctx.weekly_poll_trigger {
                                    weekly_poll::handle_interaction(
                                        trigger,
                                        &v["d"],
                                        ctx.discord.as_ref(),
                                        &ctx.log,
                                    );
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

async fn handle_message_create(ctx: &GatewayCtx, d: &Value) {
    let channel_id_str = d["channel_id"].as_str().unwrap_or("").to_string();
    let message_id_str = d["id"].as_str().unwrap_or("").to_string();

    // Log to DB first (any channel/guild), then fire the poll trigger
    // for the configured channel.
    if let Some(db) = &ctx.db {
        log_create_to_db(ctx, db, d).await;
    }

    // Invite-link moderation (any channel). Spawns its own task; never
    // blocks the receive loop.
    if let Some(moderator) = &ctx.moderation {
        moderation::inspect_create(moderator, d);
    }

    if !channel_id_str.is_empty() && channel_id_str == ctx.channel_id && !message_id_str.is_empty()
    {
        push(
            &ctx.log,
            &format!("gateway: new message {message_id_str} — triggering fetch"),
        )
        .await;
        ctx.poll_trigger.notify_one();
    }
}

async fn handle_message_update(ctx: &GatewayCtx, d: &Value) {
    let channel_id_str = d["channel_id"].as_str().unwrap_or("").to_string();
    let message_id_str = d["id"].as_str().unwrap_or("").to_string();

    if let Some(db) = &ctx.db
        && let (Some(mid), Some(cid)) = (parse_id(&message_id_str), parse_id(&channel_id_str))
        && let Some(content) = d.get("content").and_then(|c| c.as_str())
    {
        let guild_id = d["guild_id"].as_str().and_then(parse_id);
        db.log_edit(mid, cid, guild_id, content).await;
    }

    if !channel_id_str.is_empty() && channel_id_str == ctx.channel_id && !message_id_str.is_empty()
    {
        push(
            &ctx.log,
            &format!("gateway: edit detected on {message_id_str}"),
        )
        .await;
        if let Some(tx) = &ctx.edit_tx {
            let _ = tx.send(message_id_str);
        }
    }
}

async fn handle_message_delete(ctx: &GatewayCtx, d: &Value) {
    let channel_id_str = d["channel_id"].as_str().unwrap_or("").to_string();
    let message_id_str = d["id"].as_str().unwrap_or("").to_string();

    if let Some(db) = &ctx.db
        && let (Some(mid), Some(cid)) = (parse_id(&message_id_str), parse_id(&channel_id_str))
    {
        let guild_id = d["guild_id"].as_str().and_then(parse_id);
        db.log_delete(mid, cid, guild_id).await;
        push(
            &ctx.log,
            &format!("gateway: delete logged {message_id_str}"),
        )
        .await;
    }
}

async fn log_create_to_db(ctx: &GatewayCtx, db: &Db, d: &Value) {
    // Bots can fire MESSAGE_CREATE too (incl. webhooks). Skip them so the
    // logger captures actual user activity and the messages table doesn't
    // bloat with bot chatter.
    let is_bot = d["author"]["bot"].as_bool().unwrap_or(false);
    if is_bot {
        return;
    }

    let Some(message_id) = d["id"].as_str().and_then(parse_id) else { return };
    let Some(channel_id) = d["channel_id"].as_str().and_then(parse_id) else { return };
    let Some(user_id) = d["author"]["id"].as_str().and_then(parse_id) else { return };
    let username = d["author"]["username"].as_str().unwrap_or("");
    let content = d["content"].as_str().unwrap_or("");
    let guild_id_str = d["guild_id"].as_str();
    let guild_id = guild_id_str.and_then(parse_id);
    let nickname = d["member"]["nick"].as_str();

    let created_at = d["timestamp"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    db.log_user(user_id, username).await;

    if let (Some(gid), Some(gid_str)) = (guild_id, guild_id_str) {
        // Resolve guild name via REST. On failure, fall back to a placeholder
        // so we still get a row to join against.
        let name = match &ctx.discord {
            Some(c) => match c.fetch_guild_name(gid_str).await {
                Ok(n) => n,
                Err(_) => format!("guild:{gid_str}"),
            },
            None => format!("guild:{gid_str}"),
        };
        db.log_guild(gid, &name).await;
        db.log_member(user_id, gid, nickname).await;
    }

    db.log_message(message_id, user_id, channel_id, guild_id, content, created_at)
        .await;
}

fn parse_id(s: &str) -> Option<i64> {
    s.parse::<u64>().ok().map(|n| n as i64)
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
