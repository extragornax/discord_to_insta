mod discord;
mod gateway;
mod images;
mod state;
mod transform;

use axum::{
    Json, Router,
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use discord::{Client, Message};
use serde::Serialize;
use state::AppState;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use transform::discord_to_caption;

const DEFAULT_CHANNEL_ID: &str = "981806074233507880";
const DEFAULT_GUILD_ID: &str = "981525647891525642";
const DEFAULT_IMAGES_DIR: &str = "images";
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_FETCH_LIMIT: u32 = 50;
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const REACT_DELAY: Duration = Duration::from_secs(1);
const REACTION_EMOJIS: &[&str] = &["✅", "🚫", "🤔"];
const LOG_MAX_LINES: usize = 40;
const INDEX_HTML: &str = include_str!("index.html");

/// Seeded on first launch if `AppState.handles` is empty. The user can edit
/// / remove any of these from the Répertoire panel; they're not re-added
/// unless the map is completely emptied again.
const DEFAULT_HANDLES: &[(&str, &str)] = &[
    ("699543821465419806", "bertrandbernager"),
    ("222353499638202369", "extragornax"),
    ("198518357236908033", "mithiriath"),
];

struct Config {
    token: String,
    channel_id: String,
    guild_id: String,
    images_dir: PathBuf,
    state_path: PathBuf,
}

struct PollerHandle {
    stop_flag: Arc<AtomicBool>,
}

#[derive(Clone)]
struct AppCtx {
    config: Arc<Config>,
    client: Arc<Client>,
    poller_log: Arc<Mutex<VecDeque<String>>>,
    poller: Arc<Mutex<Option<PollerHandle>>>,
    last_seen_id: Arc<Mutex<Option<String>>>,
    gateway_connected: Arc<AtomicBool>,
    /// Fired by the gateway on MESSAGE_CREATE for the target channel so the
    /// poller can react within seconds instead of waiting out its 30 s timer.
    poll_trigger: Arc<tokio::sync::Notify>,
    /// Held for the duration of any load-mutate-save cycle on state.json.
    /// The UI's handle-editor and the poller's reaction-writer both touch
    /// the same file; this serializes them so one side's changes can't
    /// stomp the other's between its read and its write.
    state_write_lock: Arc<Mutex<()>>,
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let config = Arc::new(Config {
        token: std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default(),
        channel_id: env_or("DISCORD_CHANNEL_ID", DEFAULT_CHANNEL_ID),
        guild_id: env_or("DISCORD_GUILD_ID", DEFAULT_GUILD_ID),
        images_dir: PathBuf::from(env_or("DISCORD_TO_INSTA_IMAGES_DIR", DEFAULT_IMAGES_DIR)),
        state_path: match std::env::var("DISCORD_TO_INSTA_STATE_PATH") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => state::default_path(),
        },
    });

    if config.token.is_empty() {
        eprintln!("warning: DISCORD_BOT_TOKEN is empty — fetch and auto-react will fail until set");
    }

    let mut state_data = AppState::load(&config.state_path);

    // Seed the default handle map the first time we run against a fresh state
    // file. Operator edits (including deletions) are preserved — the seed
    // only runs when the whole map is empty.
    if state_data.handles.is_empty() {
        for (id, handle) in DEFAULT_HANDLES {
            state_data
                .handles
                .insert((*id).to_string(), (*handle).to_string());
        }
        if let Err(e) = state_data.save(&config.state_path) {
            eprintln!("warning: couldn't write seed handles to state: {e}");
        } else {
            println!("seeded {} default Instagram handles", DEFAULT_HANDLES.len());
        }
    }

    let last_seen = state_data
        .last_reacted_by_channel
        .get(&config.channel_id)
        .cloned();

    let client = Arc::new(Client::new(config.token.clone()));
    let ctx = AppCtx {
        config: config.clone(),
        client,
        poller_log: Arc::new(Mutex::new(VecDeque::new())),
        poller: Arc::new(Mutex::new(None)),
        last_seen_id: Arc::new(Mutex::new(last_seen)),
        gateway_connected: Arc::new(AtomicBool::new(false)),
        poll_trigger: Arc::new(tokio::sync::Notify::new()),
        state_write_lock: Arc::new(Mutex::new(())),
    };

    // Gateway task: holds a WebSocket to Discord so the bot shows as online,
    // and fires ctx.poll_trigger on every MESSAGE_CREATE in our channel so
    // the poller can react within seconds instead of waiting out its timer.
    if !config.token.is_empty() {
        let gw_ctx = gateway::GatewayCtx {
            token: config.token.clone(),
            channel_id: config.channel_id.clone(),
            stop_flag: Arc::new(AtomicBool::new(false)), // lives for the process
            log: ctx.poller_log.clone(),
            connected: ctx.gateway_connected.clone(),
            poll_trigger: ctx.poll_trigger.clone(),
        };
        tokio::spawn(gateway::run(gw_ctx));
    }

    // Auto-start the poller so the bot keeps reacting even when no operator
    // is watching. Skipped silently if the token is empty — starting would
    // just error-log every 30 s.
    if !config.token.is_empty() {
        start_poller(&ctx).await;
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(api_config))
        .route("/api/fetch", post(api_fetch))
        .route("/api/preview", post(api_preview))
        .route("/api/handles", get(api_handles_get).post(api_handles_post))
        .route("/api/poller/toggle", post(api_poller_toggle))
        .route("/api/poller/status", get(api_poller_status))
        .route("/api/poller/running", get(api_poller_running))
        .route("/api/poller/log", get(api_poller_log))
        .route("/api/gateway/status", get(api_gateway_status))
        .nest_service("/images", ServeDir::new(config.images_dir.clone()))
        .with_state(ctx);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    println!("discord_to_insta listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("axum server");
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("received SIGINT, shutting down");
}

// ---------- handlers ----------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Serialize)]
struct ConfigDto {
    channel_id: String,
    guild_id: String,
}

async fn api_config(State(ctx): State<AppCtx>) -> Json<ConfigDto> {
    Json(ConfigDto {
        channel_id: ctx.config.channel_id.clone(),
        guild_id: ctx.config.guild_id.clone(),
    })
}

async fn api_fetch(State(ctx): State<AppCtx>) -> Result<Html<String>, AppError> {
    let messages = ctx
        .client
        .fetch_messages(&ctx.config.channel_id, DEFAULT_FETCH_LIMIT)
        .await?;
    Ok(Html(render_message_list(&messages)))
}

async fn api_preview(
    State(ctx): State<AppCtx>,
    Form(form): Form<HashMap<String, String>>,
) -> Html<String> {
    let raw = form.get("raw").cloned().unwrap_or_default();
    // Handles come from the persistent store now — the Répertoire panel edits
    // them via /api/handles, not via the preview form.
    let user_map = AppState::load(&ctx.config.state_path).handles;
    let caption = discord_to_caption(&raw, &user_map);
    let distance_km = images::parse_distance_km(&raw);
    let image_path = distance_km
        .and_then(|km| images::image_for_distance(ctx.config.images_dir.as_path(), km));

    // Two fragments via OOB swap: the caption textarea (main target) and the
    // image panel (out-of-band).
    let caption_html = format!(
        r#"<textarea readonly rows="26">{}</textarea>"#,
        html_escape(&caption)
    );

    let image_html = match (distance_km, &image_path) {
        (Some(km), Some(p)) => {
            let filename = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            format!(
                r#"<div id="image-preview" class="image-preview" hx-swap-oob="true">
                    <div><span class="badge ok">✓ {km} km</span></div>
                    <p class="muted" style="font-size:11px;word-break:break-all">{file}</p>
                    <img src="/images/{file}" alt="post template for {km} km">
                </div>"#,
                km = km,
                file = html_escape(filename)
            )
        }
        (Some(km), None) => format!(
            r#"<div id="image-preview" class="image-preview" hx-swap-oob="true">
                <div><span class="badge warn">⚠ {km} km — no matching image</span></div>
                <p class="muted" style="font-size:11px">expected <code>*_{km}.png</code> in <code>{dir}/</code></p>
            </div>"#,
            km = km,
            dir = html_escape(&ctx.config.images_dir.display().to_string())
        ),
        (None, _) => format!(
            r#"<div id="image-preview" class="image-preview" hx-swap-oob="true">
                <p class="muted">No distance detected in the message. Caption must contain <code>Distance : Nkm</code>.</p>
            </div>"#,
        ),
    };

    Html(format!("{caption_html}{image_html}"))
}

async fn api_poller_toggle(
    State(ctx): State<AppCtx>,
    Form(form): Form<HashMap<String, String>>,
) -> Html<String> {
    let enable = matches!(form.get("enabled").map(|s| s.as_str()), Some("1" | "true" | "on"));
    if enable {
        start_poller(&ctx).await;
    } else {
        stop_poller(&ctx).await;
    }
    render_poller_status(&ctx).await
}

async fn api_poller_status(State(ctx): State<AppCtx>) -> Html<String> {
    render_poller_status(&ctx).await
}

async fn api_poller_running(State(ctx): State<AppCtx>) -> &'static str {
    if ctx.poller.lock().await.is_some() {
        "1"
    } else {
        "0"
    }
}

async fn api_gateway_status(State(ctx): State<AppCtx>) -> Html<&'static str> {
    if ctx.gateway_connected.load(Ordering::Relaxed) {
        Html(r#"<span class="badge ok">online</span>"#)
    } else {
        Html(r#"<span class="badge muted">offline</span>"#)
    }
}

async fn api_handles_get(State(ctx): State<AppCtx>) -> Html<String> {
    let state = AppState::load(&ctx.config.state_path);
    Html(render_handle_rows(&state.handles))
}

async fn api_handles_post(
    State(ctx): State<AppCtx>,
    Form(form): Form<HashMap<String, String>>,
) -> Html<&'static str> {
    let new_handles = collect_user_map(&form);
    // Serialize the read-modify-write against the poller's own writer.
    let _guard = ctx.state_write_lock.lock().await;
    let mut state = AppState::load(&ctx.config.state_path);
    state.handles = new_handles;
    if let Err(e) = state.save(&ctx.config.state_path) {
        eprintln!("handles save failed: {e}");
    }
    // hx-swap="none" on the client — body is ignored.
    Html("ok")
}

fn render_handle_rows(handles: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = handles.iter().collect();
    // Stable order: handle alphabetically, then ID.
    pairs.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
    let mut buf = String::new();
    for (i, (id, handle)) in pairs.iter().enumerate() {
        buf.push_str(&format!(
            r#"<div class="handle-row">
                <input name="handle_id_{i}" value="{id}" placeholder="ID Discord">
                <input name="handle_user_{i}" value="{handle}" placeholder="@handle">
                <button type="button" onclick="removeHandleRow(this)">✕</button>
            </div>"#,
            i = i,
            id = html_escape(id),
            handle = html_escape(handle),
        ));
    }
    if buf.is_empty() {
        buf.push_str(
            r#"<div class="handle-row">
                <input name="handle_id_0" placeholder="ID Discord">
                <input name="handle_user_0" placeholder="@handle">
                <button type="button" onclick="removeHandleRow(this)">✕</button>
            </div>"#,
        );
    }
    buf
}

async fn api_poller_log(State(ctx): State<AppCtx>) -> Html<String> {
    let log = ctx.poller_log.lock().await;
    if log.is_empty() {
        return Html(r#"<span class="muted">No events yet. Enable auto-react to start watching.</span>"#.to_string());
    }
    let mut buf = String::new();
    for line in log.iter() {
        let class = if line.contains("error") || line.contains("❌") {
            "err"
        } else if line.starts_with("reacted") || line.contains("bootstrap") {
            "ok"
        } else {
            ""
        };
        buf.push_str(&format!(
            r#"<div class="line {class}">{}</div>"#,
            html_escape(line)
        ));
    }
    Html(buf)
}

// ---------- rendering helpers -----------------------------------------------

fn render_message_list(messages: &[Message]) -> String {
    if messages.is_empty() {
        return r#"<p class="muted">No messages in this channel.</p>"#.to_string();
    }
    let mut buf = String::new();
    for msg in messages {
        let mention_ids: Vec<&str> = msg.mentions.iter().map(|m| m.id.as_str()).collect();
        let mentions_json = serde_json::to_string(&mention_ids).unwrap_or_else(|_| "[]".into());
        let first_line = msg.content.lines().next().unwrap_or("").trim();
        let preview: String = if first_line.chars().count() > 160 {
            first_line.chars().take(160).collect::<String>() + "…"
        } else {
            first_line.to_string()
        };
        let date = msg.timestamp.split('T').next().unwrap_or(&msg.timestamp);
        buf.push_str(&format!(
            r#"<div class="msg" data-id="{id}" data-content="{content}" data-mentions='{mentions}'>
                <div class="meta">{date} · {author}</div>
                <div class="preview">{preview}</div>
            </div>"#,
            id = html_escape(&msg.id),
            content = html_attr_escape(&msg.content),
            mentions = html_attr_escape(&mentions_json),
            date = html_escape(date),
            author = html_escape(msg.author.display()),
            preview = html_escape(&preview),
        ));
    }
    buf
}

async fn render_poller_status(ctx: &AppCtx) -> Html<String> {
    let running = ctx.poller.lock().await.is_some();
    let last_seen = ctx.last_seen_id.lock().await.clone();
    let last = last_seen.as_deref().unwrap_or("none");
    let (class, label) = if running { ("ok", "running") } else { ("muted", "stopped") };
    Html(format!(
        r#"<span class="badge {class}">{label}</span> <span class="muted">last seen:</span> <code style="font-size:11px">{last}</code>"#,
        last = html_escape(last)
    ))
}

fn collect_user_map(form: &HashMap<String, String>) -> HashMap<String, String> {
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut handles: HashMap<String, String> = HashMap::new();
    for (k, v) in form {
        if let Some(n) = k.strip_prefix("handle_id_") {
            if !v.trim().is_empty() {
                ids.insert(n.to_string(), v.trim().to_string());
            }
        } else if let Some(n) = k.strip_prefix("handle_user_") {
            if !v.trim().is_empty() {
                handles.insert(n.to_string(), v.trim().trim_start_matches('@').to_string());
            }
        }
    }
    ids.into_iter()
        .filter_map(|(n, id)| handles.get(&n).map(|h| (id, h.clone())))
        .collect()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_attr_escape(s: &str) -> String {
    html_escape(s)
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

// ---------- poller ----------------------------------------------------------

async fn start_poller(ctx: &AppCtx) {
    let mut guard = ctx.poller.lock().await;
    if guard.is_some() {
        return;
    }
    if ctx.config.token.is_empty() {
        push_log(
            &ctx.poller_log,
            "cannot start auto-react: DISCORD_BOT_TOKEN is empty",
        )
        .await;
        return;
    }
    let stop_flag = Arc::new(AtomicBool::new(false));
    *guard = Some(PollerHandle {
        stop_flag: stop_flag.clone(),
    });
    drop(guard);

    push_log(
        &ctx.poller_log,
        &format!(
            "auto-react started (every {}s; emojis {})",
            POLL_INTERVAL.as_secs(),
            REACTION_EMOJIS.join(" ")
        ),
    )
    .await;

    let ctx = ctx.clone();
    tokio::spawn(async move { run_poller(ctx, stop_flag).await });
}

async fn stop_poller(ctx: &AppCtx) {
    let mut guard = ctx.poller.lock().await;
    if let Some(handle) = guard.take() {
        handle.stop_flag.store(true, Ordering::Relaxed);
        drop(guard);
        push_log(&ctx.poller_log, "auto-react stopping…").await;
    }
}

async fn run_poller(ctx: AppCtx, stop_flag: Arc<AtomicBool>) {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            push_log(&ctx.poller_log, "auto-react stopped").await;
            *ctx.poller.lock().await = None;
            return;
        }

        match ctx
            .client
            .fetch_messages(&ctx.config.channel_id, DEFAULT_FETCH_LIMIT)
            .await
        {
            Ok(messages) => {
                if let Err(e) = handle_batch(&ctx, messages).await {
                    push_log(&ctx.poller_log, &format!("error: {e}")).await;
                }
            }
            Err(e) => {
                push_log(&ctx.poller_log, &format!("error: {e}")).await;
            }
        }

        // Wait up to POLL_INTERVAL, but cut short if the gateway spotted a
        // MESSAGE_CREATE in our channel (fast path) or if we're asked to stop.
        let mut slept = Duration::ZERO;
        while slept < POLL_INTERVAL {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let step = Duration::from_millis(200);
            tokio::select! {
                _ = tokio::time::sleep(step) => {
                    slept += step;
                }
                _ = ctx.poll_trigger.notified() => {
                    push_log(&ctx.poller_log, "gateway trigger: fetching early").await;
                    break;
                }
            }
        }
    }
}

async fn handle_batch(ctx: &AppCtx, messages: Vec<Message>) -> std::io::Result<()> {
    // Serialize with the Répertoire's writer so handles edited mid-poll
    // don't get clobbered by a concurrent poller save.
    let _guard = ctx.state_write_lock.lock().await;
    let mut persisted = AppState::load(&ctx.config.state_path);
    let last_seen = persisted
        .last_reacted_by_channel
        .get(&ctx.config.channel_id)
        .cloned();

    if messages.is_empty() {
        return Ok(());
    }

    if last_seen.is_none() {
        let newest = messages[0].id.clone();
        persisted
            .last_reacted_by_channel
            .insert(ctx.config.channel_id.clone(), newest.clone());
        persisted.save(&ctx.config.state_path)?;
        *ctx.last_seen_id.lock().await = Some(newest.clone());
        push_log(
            &ctx.poller_log,
            &format!("bootstrap: recorded newest {newest} (no retroactive reactions)"),
        )
        .await;
        return Ok(());
    }

    let last = last_seen.unwrap();
    let mut new_msgs: Vec<Message> = messages
        .into_iter()
        .filter(|m| state::is_newer_snowflake(&m.id, &last))
        .collect();
    new_msgs.reverse(); // process oldest-first

    for msg in new_msgs {
        let mut all_done = true;
        for emoji in REACTION_EMOJIS {
            // Skip emojis already recorded as placed — survives restarts and
            // crash-mid-batch without re-hitting the API.
            if persisted.has_reacted(&ctx.config.channel_id, &msg.id, emoji) {
                continue;
            }
            match ctx
                .client
                .add_reaction(&ctx.config.channel_id, &msg.id, emoji)
                .await
            {
                Ok(()) => {
                    persisted.record_reaction(&ctx.config.channel_id, &msg.id, emoji);
                    // Persist after every successful reaction so an
                    // interrupted batch picks up exactly where it stopped.
                    persisted.save(&ctx.config.state_path)?;
                    push_log(
                        &ctx.poller_log,
                        &format!("reacted {emoji} on {}", msg.id),
                    )
                    .await;
                }
                Err(e) => {
                    push_log(
                        &ctx.poller_log,
                        &format!("error reacting {emoji} on {}: {e}", msg.id),
                    )
                    .await;
                    all_done = false;
                    break;
                }
            }
            // Deliberate pacing — Discord's message-reaction bucket is
            // tight (roughly 1 reaction/sec per channel); this keeps us
            // comfortably under without hitting 429s.
            tokio::time::sleep(REACT_DELAY).await;
        }
        if all_done {
            persisted
                .last_reacted_by_channel
                .insert(ctx.config.channel_id.clone(), msg.id.clone());
            persisted.clear_reactions(&ctx.config.channel_id, &msg.id);
            persisted.save(&ctx.config.state_path)?;
            *ctx.last_seen_id.lock().await = Some(msg.id.clone());
        } else {
            break;
        }
    }
    Ok(())
}

async fn push_log(log: &Mutex<VecDeque<String>>, line: &str) {
    let mut l = log.lock().await;
    if l.len() >= LOG_MAX_LINES {
        l.pop_front();
    }
    l.push_back(line.to_string());
}

// ---------- error type ------------------------------------------------------

struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!(
                r#"<div class="badge err">error</div> <span>{}</span>"#,
                html_escape(&self.0)
            )),
        )
            .into_response()
    }
}

impl From<discord::Error> for AppError {
    fn from(value: discord::Error) -> Self {
        Self(value.to_string())
    }
}
