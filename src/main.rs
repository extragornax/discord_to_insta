mod discord;
mod images;
mod state;
mod transform;

use discord::{Client, Message};
use eframe::egui;
use state::AppState;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use transform::discord_to_caption;

const DEFAULT_CHANNEL_ID: &str = "981806074233507880"; // Mayo Jaune announcements
const DEFAULT_FETCH_LIMIT: u32 = 50;
const IMAGES_DIR: &str = "images";
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const REACTION_EMOJIS: &[&str] = &["✅", "🚫", "🤔"];
const LOG_MAX_LINES: usize = 20;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]),
        ..Default::default()
    };
    eframe::run_native(
        "discord_to_insta",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

type FetchResult = Result<Vec<Message>, discord::Error>;

enum PollerEvent {
    Bootstrap { last_id: String },
    Reacted { message_id: String, emoji: String },
    NoNewMessages,
    Error(String),
    Stopped,
}

struct PollerHandle {
    stop_flag: Arc<AtomicBool>,
}

impl PollerHandle {
    fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

struct App {
    token: String,
    channel_id: String,
    raw: String,
    user_map_rows: Vec<(String, String)>,
    messages: Vec<Message>,
    selected_message_id: Option<String>,
    fetch_rx: Option<Receiver<FetchResult>>,
    fetch_status: Status,
    last_copy_status: Option<String>,
    state_path: PathBuf,
    poller: Option<PollerHandle>,
    poller_rx: Option<Receiver<PollerEvent>>,
    poller_log: VecDeque<String>,
    last_seen_id: Option<String>,
}

#[derive(Default, Clone)]
enum Status {
    #[default]
    Idle,
    Fetching,
    Error(String),
    Ok(String),
}

impl App {
    fn new() -> Self {
        let token = std::env::var("DISCORD_BOT_TOKEN").unwrap_or_default();
        let state_path = state::default_path();
        let state = AppState::load(&state_path);
        let channel_id = DEFAULT_CHANNEL_ID.to_string();
        let last_seen_id = state.last_reacted_by_channel.get(&channel_id).cloned();
        Self {
            token,
            channel_id,
            raw: String::new(),
            user_map_rows: vec![(String::new(), String::new())],
            messages: Vec::new(),
            selected_message_id: None,
            fetch_rx: None,
            fetch_status: Status::Idle,
            last_copy_status: None,
            state_path,
            poller: None,
            poller_rx: None,
            poller_log: VecDeque::new(),
            last_seen_id,
        }
    }

    fn user_map(&self) -> HashMap<String, String> {
        self.user_map_rows
            .iter()
            .filter(|(id, handle)| !id.trim().is_empty() && !handle.trim().is_empty())
            .map(|(id, handle)| {
                (
                    id.trim().to_string(),
                    handle.trim().trim_start_matches('@').to_string(),
                )
            })
            .collect()
    }

    fn start_fetch(&mut self) {
        if matches!(self.fetch_status, Status::Fetching) {
            return;
        }
        if self.token.trim().is_empty() {
            self.fetch_status =
                Status::Error("Missing bot token (set DISCORD_BOT_TOKEN or paste it above).".into());
            return;
        }
        if self.channel_id.trim().is_empty() {
            self.fetch_status = Status::Error("Missing channel ID.".into());
            return;
        }

        let (tx, rx) = channel();
        let token = self.token.trim().to_string();
        let channel_id = self.channel_id.trim().to_string();
        thread::spawn(move || {
            let client = Client::new(token);
            let result = client.fetch_messages(&channel_id, DEFAULT_FETCH_LIMIT);
            let _ = tx.send(result);
        });
        self.fetch_rx = Some(rx);
        self.fetch_status = Status::Fetching;
    }

    fn poll_fetch(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.fetch_rx else { return };
        match rx.try_recv() {
            Ok(Ok(messages)) => {
                let n = messages.len();
                self.messages = messages;
                self.fetch_status = Status::Ok(format!("fetched {n} messages"));
                self.fetch_rx = None;
            }
            Ok(Err(e)) => {
                self.fetch_status = Status::Error(e.to_string());
                self.fetch_rx = None;
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(TryRecvError::Disconnected) => {
                self.fetch_status = Status::Error("Fetch thread disconnected.".into());
                self.fetch_rx = None;
            }
        }
    }

    fn load_selected(&mut self, id: &str) {
        if let Some(m) = self.messages.iter().find(|m| m.id == id) {
            self.raw = m.content.clone();
            self.selected_message_id = Some(id.to_string());
            for user in &m.mentions {
                let already_present = self.user_map_rows.iter().any(|(rid, _)| rid == &user.id);
                if !already_present {
                    self.user_map_rows.push((user.id.clone(), String::new()));
                }
            }
        }
    }

    fn start_poller(&mut self) {
        if self.poller.is_some() {
            return;
        }
        if self.token.trim().is_empty() {
            self.push_log("cannot start auto-react: missing bot token".into());
            return;
        }
        if self.channel_id.trim().is_empty() {
            self.push_log("cannot start auto-react: missing channel ID".into());
            return;
        }

        let (tx, rx) = channel();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let token = self.token.trim().to_string();
        let channel_id = self.channel_id.trim().to_string();
        let state_path = self.state_path.clone();
        let stop_clone = stop_flag.clone();
        thread::spawn(move || run_poller(token, channel_id, state_path, stop_clone, tx));
        self.poller = Some(PollerHandle { stop_flag });
        self.poller_rx = Some(rx);
        self.push_log(format!(
            "auto-react started (polling every {}s, emojis: {})",
            POLL_INTERVAL.as_secs(),
            REACTION_EMOJIS.join(" ")
        ));
    }

    fn stop_poller(&mut self) {
        if let Some(handle) = self.poller.take() {
            handle.request_stop();
            self.push_log("auto-react stopping…".into());
        }
    }

    fn poll_poller_events(&mut self, ctx: &egui::Context) {
        // Drain first, handle after, so we don't hold a borrow of self while
        // calling &mut self methods.
        let mut drained: Vec<PollerEvent> = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.poller_rx {
            loop {
                match rx.try_recv() {
                    Ok(ev) => drained.push(ev),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for ev in drained {
            self.handle_poller_event(ev);
        }
        if disconnected {
            self.poller_rx = None;
            self.poller = None;
            self.push_log("auto-react thread ended".into());
        } else if self.poller.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn handle_poller_event(&mut self, ev: PollerEvent) {
        match ev {
            PollerEvent::Bootstrap { last_id } => {
                self.last_seen_id = Some(last_id.clone());
                self.push_log(format!(
                    "bootstrap: recorded newest message {last_id} (no retroactive reactions)"
                ));
            }
            PollerEvent::Reacted { message_id, emoji } => {
                self.last_seen_id = Some(message_id.clone());
                self.push_log(format!("reacted {emoji} on {message_id}"));
            }
            PollerEvent::NoNewMessages => {
                // Chatty; skip.
            }
            PollerEvent::Error(e) => {
                self.push_log(format!("error: {e}"));
            }
            PollerEvent::Stopped => {
                self.poller = None;
                self.poller_rx = None;
                self.push_log("auto-react stopped".into());
            }
        }
    }

    fn push_log(&mut self, line: String) {
        if self.poller_log.len() >= LOG_MAX_LINES {
            self.poller_log.pop_front();
        }
        self.poller_log.push_back(line);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_fetch(ctx);
        self.poll_poller_events(ctx);

        egui::TopBottomPanel::top("config").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Bot token:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.token)
                        .password(true)
                        .hint_text("Bot token (or set DISCORD_BOT_TOKEN)")
                        .desired_width(260.0),
                );
                ui.label("Channel ID:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.channel_id)
                        .hint_text("Discord channel ID")
                        .desired_width(200.0),
                );
                let fetching = matches!(self.fetch_status, Status::Fetching);
                let btn = ui.add_enabled(
                    !fetching,
                    egui::Button::new(if fetching { "Fetching…" } else { "Fetch recent" }),
                );
                if btn.clicked() {
                    self.start_fetch();
                }
                match &self.fetch_status {
                    Status::Idle => {}
                    Status::Fetching => {
                        ui.spinner();
                    }
                    Status::Ok(msg) => {
                        ui.colored_label(egui::Color32::from_rgb(80, 180, 100), msg);
                    }
                    Status::Error(msg) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 100, 100), msg);
                    }
                }
            });

            ui.horizontal(|ui| {
                let running = self.poller.is_some();
                let mut enabled = running;
                if ui
                    .checkbox(&mut enabled, "Auto-react to new announcements")
                    .changed()
                {
                    if enabled {
                        self.start_poller();
                    } else {
                        self.stop_poller();
                    }
                }
                ui.label(format!("emojis: {}", REACTION_EMOJIS.join(" ")));
                ui.separator();
                match &self.last_seen_id {
                    Some(id) => ui.label(format!("last seen: {id}")),
                    None => ui.label("last seen: (none — first run)"),
                };
                if running {
                    ui.spinner();
                }
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(110.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Auto-react log").strong());
                egui::ScrollArea::vertical()
                    .id_salt("log_scroll")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.poller_log.is_empty() {
                            ui.label("(no events yet)");
                        } else {
                            for line in &self.poller_log {
                                ui.label(line);
                            }
                        }
                    });
            });

        egui::SidePanel::left("messages")
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("Recent messages");
                ui.separator();
                if self.messages.is_empty() {
                    ui.label("No messages fetched yet.");
                } else {
                    let selected = self.selected_message_id.clone();
                    let mut to_load: Option<String> = None;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for msg in &self.messages {
                            let is_selected = selected.as_deref() == Some(&msg.id);
                            let preview = message_preview(&msg.content);
                            let label = format!(
                                "{}  {}\n{}",
                                short_timestamp(&msg.timestamp),
                                msg.author.display(),
                                preview
                            );
                            if ui.selectable_label(is_selected, label).clicked() {
                                to_load = Some(msg.id.clone());
                            }
                            ui.separator();
                        }
                    });
                    if let Some(id) = to_load {
                        self.load_selected(&id);
                    }
                }
            });

        egui::SidePanel::right("handles")
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("Discord ID → Instagram handle");
                ui.label("Mentions not in this table are removed.");
                ui.separator();

                let mut remove_idx: Option<usize> = None;
                for (i, (id, handle)) in self.user_map_rows.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(id)
                                .hint_text("123456789")
                                .desired_width(150.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(handle)
                                .hint_text("@handle")
                                .desired_width(120.0),
                        );
                        if ui.small_button("✕").clicked() {
                            remove_idx = Some(i);
                        }
                    });
                }
                if let Some(i) = remove_idx {
                    self.user_map_rows.remove(i);
                }
                if ui.button("+ add row").clicked() {
                    self.user_map_rows.push((String::new(), String::new()));
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let caption = discord_to_caption(&self.raw, &self.user_map());
            let distance_km = images::parse_distance_km(&self.raw);
            let image_path = distance_km
                .and_then(|km| images::image_for_distance(Path::new(IMAGES_DIR), km));

            ui.heading("discord_to_insta");
            ui.horizontal(|ui| {
                ui.label("Image:");
                match (distance_km, &image_path) {
                    (Some(km), Some(p)) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(80, 180, 100),
                            format!("✓ {} km → {}", km, display_path(p)),
                        );
                    }
                    (Some(km), None) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 60),
                            format!(
                                "⚠ {} km detected but no image found in {}/ (expected *_{}.png)",
                                km, IMAGES_DIR, km
                            ),
                        );
                    }
                    (None, _) => {
                        ui.label("(no distance detected — caption must contain 'Distance : Nkm')");
                    }
                }
            });
            ui.separator();

            let available = ui.available_size();
            let preview_width = 260.0;
            let col_width = (available.x - preview_width - 24.0) / 2.0;
            let col_height = available.y - 40.0;

            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label("Discord source");
                    egui::ScrollArea::vertical()
                        .id_salt("src")
                        .max_height(col_height)
                        .show(ui, |ui| {
                            ui.add_sized(
                                [col_width, col_height],
                                egui::TextEdit::multiline(&mut self.raw)
                                    .hint_text("Pick a message on the left, or paste one here…"),
                            );
                        });
                });

                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Instagram caption");
                        if ui.button("Copy").clicked() {
                            ctx.copy_text(caption.clone());
                            self.last_copy_status =
                                Some(format!("copied {} chars", caption.chars().count()));
                        }
                        if let Some(status) = &self.last_copy_status {
                            ui.small(status);
                        }
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("out")
                        .max_height(col_height)
                        .show(ui, |ui| {
                            let mut view = caption.clone();
                            ui.add_sized(
                                [col_width, col_height],
                                egui::TextEdit::multiline(&mut view).interactive(false),
                            );
                        });
                });

                ui.vertical(|ui| {
                    ui.label("Post image preview");
                    match &image_path {
                        Some(p) => {
                            let uri = format!("file://{}", p.display());
                            ui.add(
                                egui::Image::from_uri(uri)
                                    .max_width(preview_width)
                                    .maintain_aspect_ratio(true)
                                    .fit_to_original_size(1.0),
                            );
                        }
                        None => {
                            ui.allocate_space(egui::vec2(preview_width, col_height));
                        }
                    }
                });
            });
        });
    }
}

/// Background poller: every POLL_INTERVAL, fetch recent messages, compare
/// against the persisted last-reacted-ID for this channel, and react to any
/// newer ones with all REACTION_EMOJIS. Bootstraps silently on first run.
fn run_poller(
    token: String,
    channel_id: String,
    state_path: PathBuf,
    stop_flag: Arc<AtomicBool>,
    tx: Sender<PollerEvent>,
) {
    let client = Client::new(token);

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            let _ = tx.send(PollerEvent::Stopped);
            return;
        }

        let mut state = AppState::load(&state_path);
        let last_seen = state.last_reacted_by_channel.get(&channel_id).cloned();

        match client.fetch_messages(&channel_id, DEFAULT_FETCH_LIMIT) {
            Ok(messages) => {
                if messages.is_empty() {
                    // Nothing to do.
                } else if last_seen.is_none() {
                    // Bootstrap: record the newest ID, don't react retroactively.
                    let newest = &messages[0].id;
                    state
                        .last_reacted_by_channel
                        .insert(channel_id.clone(), newest.clone());
                    if let Err(e) = state.save(&state_path) {
                        let _ = tx.send(PollerEvent::Error(format!(
                            "failed to write state {}: {e}",
                            state_path.display()
                        )));
                    } else {
                        let _ = tx.send(PollerEvent::Bootstrap {
                            last_id: newest.clone(),
                        });
                    }
                } else {
                    let last = last_seen.unwrap();
                    // Messages come newest-first; process oldest-first so
                    // last_seen advances monotonically and a mid-batch crash
                    // resumes cleanly.
                    let mut new_msgs: Vec<Message> = messages
                        .into_iter()
                        .filter(|m| state::is_newer_snowflake(&m.id, &last))
                        .collect();
                    new_msgs.reverse();

                    if new_msgs.is_empty() {
                        let _ = tx.send(PollerEvent::NoNewMessages);
                    }

                    for msg in new_msgs {
                        if stop_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        let mut all_ok = true;
                        for emoji in REACTION_EMOJIS {
                            if let Err(e) = client.add_reaction(&channel_id, &msg.id, emoji) {
                                let _ = tx.send(PollerEvent::Error(format!(
                                    "{emoji} on {}: {e}",
                                    msg.id
                                )));
                                all_ok = false;
                                break;
                            }
                            let _ = tx.send(PollerEvent::Reacted {
                                message_id: msg.id.clone(),
                                emoji: (*emoji).to_string(),
                            });
                        }
                        if all_ok {
                            state
                                .last_reacted_by_channel
                                .insert(channel_id.clone(), msg.id.clone());
                            if let Err(e) = state.save(&state_path) {
                                let _ = tx.send(PollerEvent::Error(format!(
                                    "failed to write state: {e}"
                                )));
                            }
                        } else {
                            // Stop the batch — next cycle will retry from the
                            // same last_seen, which is correct.
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(PollerEvent::Error(e.to_string()));
            }
        }

        // Interruptible sleep so stop requests are seen within ~100 ms.
        let mut slept = Duration::ZERO;
        while slept < POLL_INTERVAL {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
            slept += Duration::from_millis(100);
        }
    }
}

fn display_path(p: &PathBuf) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string())
        .unwrap_or_else(|| p.display().to_string())
}

fn message_preview(content: &str) -> String {
    const MAX: usize = 120;
    let first_line = content.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= MAX {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

fn short_timestamp(ts: &str) -> &str {
    ts.split('T').next().unwrap_or(ts)
}
