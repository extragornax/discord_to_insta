mod discord;
mod transform;

use discord::{Client, Message};
use eframe::egui;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;
use transform::discord_to_caption;

const DEFAULT_CHANNEL_ID: &str = "981806074233507880"; // Mayo Jaune announcements
const DEFAULT_FETCH_LIMIT: u32 = 50;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "discord_to_insta",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

type FetchResult = Result<Vec<Message>, discord::Error>;

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
        Self {
            token,
            channel_id: DEFAULT_CHANNEL_ID.to_string(),
            raw: String::new(),
            user_map_rows: vec![(String::new(), String::new())],
            messages: Vec::new(),
            selected_message_id: None,
            fetch_rx: None,
            fetch_status: Status::Idle,
            last_copy_status: None,
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
            self.fetch_status = Status::Error("Missing bot token (set DISCORD_BOT_TOKEN or paste it above).".into());
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
                // Keep repainting so we notice when it finishes.
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
            // Seed the handle table with the message's mentioned users so the
            // user can fill in Instagram handles without retyping Discord IDs.
            for user in &m.mentions {
                let already_present = self.user_map_rows.iter().any(|(rid, _)| rid == &user.id);
                if !already_present {
                    self.user_map_rows.push((user.id.clone(), String::new()));
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_fetch(ctx);

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
                let btn = ui.add_enabled(!fetching, egui::Button::new(if fetching { "Fetching…" } else { "Fetch recent" }));
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
            ui.add_space(4.0);
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
                        ui.add(egui::TextEdit::singleline(id).hint_text("123456789").desired_width(150.0));
                        ui.add(egui::TextEdit::singleline(handle).hint_text("@handle").desired_width(120.0));
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

            ui.heading("discord_to_insta");
            ui.separator();

            let available = ui.available_size();
            let col_width = (available.x - 16.0) / 2.0;
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
            });
        });
    }
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
    // Discord returns ISO 8601; show the date portion.
    ts.split('T').next().unwrap_or(ts)
}
