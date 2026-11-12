//! The library window: connection state, numbered library list, and the
//! status strip (design-m5 §5). All pipe traffic runs on one worker
//! thread (D11c) — the UI thread only sends commands and drains replies,
//! so a wedged or absent daemon can never freeze a frame.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use cascade_core::client::{self, ClientError};
use cascade_core::ipc::{
    no_data, LibraryAddData, LibraryAddResult, LibraryItemsData, LibraryRemoveData, RejectedPath,
    SetData, SetReport, Settings, Status,
};
use cascade_core::library::LibraryItem;
use eframe::egui;

use crate::theme;

/// Refresh cadence while the window draws (design-m5 §8: the strip polls
/// `status` every 1 s, never faster — AGENTS §13).
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// While the engine is down every poll pays the connect budget
/// (3 × 250 ms, `ipc.md` §7) inside the worker, so poll at half rate.
const POLL_INTERVAL_DISCONNECTED: Duration = Duration::from_secs(2);

/// Commands UI → worker. The worker runs them strictly in order, so at
/// most one `Poll` is ever in flight (`poll_outstanding`) and replies
/// can never interleave.
enum Cmd {
    Poll,
    Add(Vec<String>),
    Remove(String),
    Set(String),
    Resume,
    StartEngine,
}

/// Replies worker → UI.
enum Reply {
    Snapshot {
        status: Result<Status, ClientError>,
        library: Result<LibraryItemsData, ClientError>,
        settings: Result<Settings, ClientError>,
    },
    Added(Result<LibraryAddResult, ClientError>),
    /// `library.remove` / `playback.resume`: success carries no data the
    /// UI needs — the next poll shows the effect.
    Simple(Result<(), ClientError>),
    SetDone(Result<SetReport, ClientError>),
    Started(Result<(), String>),
}

/// What the last poll said about the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Conn {
    /// No answer yet (first frames).
    Unknown,
    Connected,
    NotRunning,
    /// Any other transport failure, with its human message.
    Unreachable(String),
}

pub struct CascadeApp {
    cmd: Sender<Cmd>,
    reply: Receiver<Reply>,
    conn: Conn,
    status: Option<Status>,
    settings: Option<Settings>,
    library: Vec<LibraryItem>,
    /// Stable 4-hex id — ordinals are display-only (D1: numbers never
    /// travel on the wire, they are resolved against the list on view).
    selected: Option<String>,
    /// One dismissible message: action errors, add rejections, infos.
    notice: Option<String>,
    starting: bool,
    worker_dead: bool,
    poll_at: Instant,
    poll_outstanding: bool,
    /// Guards the notice slot against the same snapshot error repeating
    /// at poll rate.
    last_snapshot_error: Option<String>,
}

impl CascadeApp {
    pub fn new() -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
        std::thread::Builder::new()
            .name("cascade-ui-ipc".to_string())
            .spawn(move || worker(cmd_rx, reply_tx))
            .map_err(|err| format!("could not start the UI connection thread: {err}"))?;
        Ok(Self {
            cmd: cmd_tx,
            reply: reply_rx,
            conn: Conn::Unknown,
            status: None,
            settings: None,
            library: Vec::new(),
            selected: None,
            notice: None,
            starting: false,
            worker_dead: false,
            poll_at: Instant::now(),
            poll_outstanding: false,
            last_snapshot_error: None,
        })
    }

    // --- worker plumbing ------------------------------------------------

    fn send(&mut self, cmd: Cmd) {
        if self.cmd.send(cmd).is_err() {
            self.mark_worker_dead();
        }
    }

    fn mark_worker_dead(&mut self) {
        if !self.worker_dead {
            self.worker_dead = true;
            self.notice = Some(
                "internal error: the UI connection thread stopped — close and reopen the window"
                    .to_string(),
            );
        }
    }

    /// Drain every pending reply, then queue the next poll when due.
    fn tick(&mut self, ctx: &egui::Context) {
        loop {
            match self.reply.try_recv() {
                Ok(reply) => self.apply(reply),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.mark_worker_dead();
                    break;
                }
            }
        }
        if !self.poll_outstanding && Instant::now() >= self.poll_at {
            if self.cmd.send(Cmd::Poll).is_ok() {
                self.poll_outstanding = true;
            } else {
                self.mark_worker_dead();
            }
        }
        // Reschedule every frame; a hidden window draws no frame, so no
        // repaint is requested and no poll is sent (architecture §9).
        ctx.request_repaint_after(POLL_INTERVAL);
    }

    /// Poll again as soon as possible (after an action, or on Retry).
    fn request_poll_now(&mut self) {
        self.poll_at = Instant::now();
    }

    fn apply(&mut self, reply: Reply) {
        match reply {
            Reply::Snapshot {
                status,
                library,
                settings,
            } => {
                self.poll_outstanding = false;
                match status {
                    Ok(status) => {
                        self.conn = Conn::Connected;
                        self.status = Some(status);
                    }
                    Err(ClientError::NotRunning) => {
                        // The engine is down: forget everything it owned
                        // so no stale list or strip values linger.
                        self.conn = Conn::NotRunning;
                        self.status = None;
                        self.settings = None;
                        self.library.clear();
                        self.selected = None;
                        self.last_snapshot_error = None;
                    }
                    Err(err) => {
                        self.conn = Conn::Unreachable(err.to_string());
                        self.status = None;
                    }
                }
                if self.conn == Conn::Connected {
                    match library {
                        Ok(data) => {
                            self.library = data.items;
                            drop_missing_selection(&self.library, &mut self.selected);
                            self.last_snapshot_error = None;
                        }
                        Err(err) => self.snapshot_error(err.to_string()),
                    }
                    match settings {
                        Ok(settings) => self.settings = Some(settings),
                        Err(err) => self.snapshot_error(err.to_string()),
                    }
                }
                let interval = if self.conn == Conn::Connected {
                    POLL_INTERVAL
                } else {
                    POLL_INTERVAL_DISCONNECTED
                };
                self.poll_at = Instant::now() + interval;
            }
            Reply::Added(result) => match result {
                Ok(data) => {
                    if !data.rejected.is_empty() {
                        self.notice = Some(format_rejections(&data.rejected));
                    }
                    self.request_poll_now();
                }
                Err(err) => self.notice = Some(err.to_string()),
            },
            Reply::Simple(result) => match result {
                Ok(()) => self.request_poll_now(),
                Err(err) => self.notice = Some(err.to_string()),
            },
            Reply::SetDone(result) => match result {
                Ok(_) => {
                    let title = self
                        .selected_item()
                        .map(|item| item.title.clone())
                        .unwrap_or_default();
                    if !title.is_empty() {
                        self.notice = Some(format!("Now playing: {title}"));
                    }
                    self.request_poll_now();
                }
                Err(err) => self.notice = Some(err.to_string()),
            },
            Reply::Started(result) => {
                self.starting = false;
                match result {
                    Ok(()) => {
                        self.notice = None;
                        // `run --detached` returns only once the pipe
                        // answers, so the next poll connects at once.
                        self.request_poll_now();
                    }
                    Err(err) => self.notice = Some(err),
                }
            }
        }
    }

    /// Show a snapshot error once, not at poll rate.
    fn snapshot_error(&mut self, message: String) {
        if self.last_snapshot_error.as_deref() != Some(message.as_str()) {
            self.last_snapshot_error = Some(message.clone());
            self.notice = Some(message);
        }
    }

    fn selected_item(&self) -> Option<&LibraryItem> {
        let id = self.selected.as_ref()?;
        self.library.iter().find(|item| &item.id == id)
    }

    /// Add via the native dialog (blocks while the modal is open).
    fn add_files(&mut self) {
        match crate::dialog::pick_files() {
            Ok(paths) if paths.is_empty() => {} // user cancelled
            Ok(paths) => self.send(Cmd::Add(paths)),
            Err(err) => self.notice = Some(err),
        }
    }

    // --- drawing --------------------------------------------------------

    /// Files dropped onto the window go through the same `library.add`
    /// as the dialog (design-m5 §5: drag-and-drop).
    fn collect_drops(&mut self, ui: &egui::Ui) {
        let dropped: Vec<String> = ui.ctx().input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_string_lossy().into_owned())
                .collect()
        });
        if !dropped.is_empty() {
            self.send(Cmd::Add(dropped));
        }
    }

    fn notice_ui(&mut self, ui: &mut egui::Ui) {
        let Some(notice) = self.notice.clone() else {
            return;
        };
        let mut dismiss = false;
        egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Notice").strong().color(theme::WARN));
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = true;
                    }
                });
                for line in notice.lines() {
                    ui.label(line);
                }
            });
        ui.add_space(6.0);
        if dismiss {
            self.notice = None;
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Cascade").strong().size(18.0));
        ui.add_space(14.0);
        // Only Library exists in P2 — Monitors/Settings arrive in P4
        // (design-m5 §5), so this entry is the current, fixed surface.
        let _ = ui.selectable_label(true, "Library");
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            theme::muted(ui, concat!("cascade ", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
        });
    }

    fn strip(&mut self, ui: &mut egui::Ui) {
        let mut resume = false;
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let (color, mode) = match &self.status {
                Some(status) => (mode_color(&status.mode), status.mode.clone()),
                None => (theme::MUTED, "offline".to_string()),
            };
            ui.colored_label(color, "\u{2022}");
            ui.label(mode);
            if let Some(status) = &self.status {
                if !status.paused_reasons.is_empty() {
                    let label = format!(
                        "Paused: {} (click to resume)",
                        status.paused_reasons.join(", ")
                    );
                    if ui.small_button(label).clicked() {
                        resume = true;
                    }
                }
            }
            ui.separator();
            ui.label(match &self.settings {
                Some(settings) => settings.engine_mode.as_str().to_string(),
                None => "\u{2014}".to_string(),
            });
            ui.separator();
            let fps = self
                .status
                .as_ref()
                .and_then(|status| status.monitors.iter().find_map(|monitor| monitor.fps));
            ui.label(match fps {
                Some(fps) => format!("{fps:.0} fps"),
                None => "\u{2014} fps".to_string(),
            });
            ui.separator();
            ui.label(match &self.status {
                Some(status) => format!("{} MB", status.memory.working_set_mb),
                None => "\u{2014} MB".to_string(),
            });
            ui.separator();
            ui.label(match &self.settings {
                Some(settings) => format!("vol {}", settings.volume),
                None => "vol \u{2014}".to_string(),
            });
        });
        if resume {
            self.send(Cmd::Resume);
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        self.notice_ui(ui);
        let conn = self.conn.clone();
        match conn {
            Conn::Unknown => {
                ui.vertical_centered(|ui| {
                    ui.add_space(70.0);
                    ui.spinner();
                    ui.add_space(8.0);
                    theme::muted(ui, "Connecting to the engine\u{2026}");
                });
            }
            Conn::NotRunning => self.start_screen(ui),
            Conn::Unreachable(message) => self.unreachable_screen(ui, &message),
            Conn::Connected => self.library_screen(ui),
        }
    }

    fn start_screen(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(70.0);
            ui.heading("Cascade is not running");
            ui.add_space(6.0);
            theme::muted(
                ui,
                "Start the engine to play a video behind your desktop icons.",
            );
            ui.add_space(18.0);
            if self.starting {
                ui.spinner();
                ui.add_space(8.0);
                theme::muted(ui, "Starting the engine\u{2026}");
            } else if theme::accent_button(ui, "Start engine").clicked() {
                self.starting = true;
                self.send(Cmd::StartEngine);
            }
            ui.add_space(12.0);
            theme::muted(ui, "or run `cascade run` in a terminal");
        });
    }

    fn unreachable_screen(&mut self, ui: &mut egui::Ui, message: &str) {
        ui.vertical_centered(|ui| {
            ui.add_space(70.0);
            ui.heading("Cannot reach the engine");
            ui.add_space(6.0);
            ui.label(egui::RichText::new(message).color(theme::ERR));
            ui.add_space(18.0);
            if theme::accent_button(ui, "Retry").clicked() {
                self.request_poll_now();
            }
        });
    }

    fn library_screen(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Library").strong().size(16.0));
            theme::muted(ui, &format!("{} video(s)", self.library.len()));
        });
        ui.add_space(8.0);

        // Toolbar directly under the header: the actions must stay
        // visible whatever the list does — a full-height ScrollArea
        // placed after them swallowed them off-panel (seen on the target
        // machine, P2b).
        let has_selection = self.selected_item().is_some();
        let mut add = false;
        let mut remove = false;
        let mut set = false;
        ui.horizontal(|ui| {
            if theme::accent_button(ui, "+ Add").clicked() {
                add = true;
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Remove"))
                .on_hover_text(
                    "Remove from the library \u{2014} the video file itself is never touched",
                )
                .clicked()
            {
                remove = true;
            }
            if ui
                .add_enabled(
                    has_selection,
                    egui::Button::new("Set as wallpaper").fill(theme::ACCENT),
                )
                .clicked()
            {
                set = true;
            }
        });
        if add {
            self.add_files();
        }
        if remove {
            if let Some(item) = self.selected_item() {
                let id = item.id.clone();
                self.send(Cmd::Remove(id));
            }
        }
        if set {
            if let Some(item) = self.selected_item() {
                let id = item.id.clone();
                self.send(Cmd::Set(id));
            }
        }
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        if self.library.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label("No videos yet.");
                ui.add_space(6.0);
                theme::muted(
                    ui,
                    "Files are referenced, never copied \u{2014} use + Add above, or drop files onto this window.",
                );
            });
            ui.add_space(28.0);
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Collected during the pass: `self.library` is borrowed
                // by the loop, so the actions land after it (edition
                // 2021 disjoint captures keep this borrow-checkable).
                let mut select: Option<String> = None;
                let mut set_row: Option<String> = None;
                for (index, item) in self.library.iter().enumerate() {
                    let selected = self.selected.as_deref() == Some(item.id.as_str());
                    let title = format!("{}. {}", index + 1, item.title);
                    let meta = item_meta(item);
                    let row = ui
                        .horizontal(|ui| {
                            let row = ui.selectable_label(selected, title);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(egui::RichText::new(meta).color(theme::MUTED));
                                },
                            );
                            row
                        })
                        .inner
                        .on_hover_text(&item.path);
                    if row.clicked() {
                        select = Some(item.id.clone());
                    }
                    if row.double_clicked() {
                        set_row = Some(item.id.clone());
                    }
                }
                if let Some(id) = select {
                    self.selected = Some(id);
                }
                if let Some(id) = set_row {
                    self.send(Cmd::Set(id));
                }
            });
    }
}

impl eframe::App for CascadeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.tick(ui.ctx());
        self.collect_drops(ui);

        egui::Panel::left("sidebar")
            .resizable(false)
            .default_size(180.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(12.0),
            )
            .show(ui, |ui| self.sidebar(ui));

        egui::Panel::bottom("status_strip")
            .resizable(false)
            .default_size(30.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(6.0),
            )
            .show(ui, |ui| self.strip(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(16.0))
            .show(ui, |ui| self.central(ui));
    }

    /// Paint the app background with the palette instead of egui's
    /// translucent default (design-m5 §5.1).
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

// --- pure helpers (unit-tested) ------------------------------------------

/// `1.2 s`-style duration in the CLI's shape: `m:ss`, `h:mm:ss` past an
/// hour (`cascade library ls` prints the same).
fn format_duration(ms: u64) -> String {
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The right-hand metadata column of a list row.
fn item_meta(item: &LibraryItem) -> String {
    let mut meta = format!("{}x{}", item.width, item.height);
    if let Some(fps) = item.fps {
        meta.push_str(&format!(" \u{b7} {fps:.0} fps"));
    }
    meta.push_str(&format!(" \u{b7} {}", item.codec));
    if let Some(ms) = item.duration_ms {
        meta.push_str(&format!(" \u{b7} {}", format_duration(ms)));
    }
    meta
}

/// One notice for a batch of rejected adds — the same words the CLI
/// prints (`cascade library add`), so both surfaces read alike.
fn format_rejections(rejected: &[RejectedPath]) -> String {
    let mut text = String::from("Not added:");
    for entry in rejected {
        text.push_str(&format!("\n{} \u{2014} {}", entry.path, entry.reason));
    }
    text
}

/// A selection whose item vanished from the list (removed elsewhere,
/// prune at startup) must not stay selected.
fn drop_missing_selection(library: &[LibraryItem], selected: &mut Option<String>) {
    let present = match selected.as_ref() {
        Some(id) => library.iter().any(|item| &item.id == id),
        None => return,
    };
    if !present {
        *selected = None;
    }
}

/// Strip dot colour for a `status.mode` string (ipc.md §4). Unknown
/// modes stay muted rather than guessing.
fn mode_color(mode: &str) -> egui::Color32 {
    match mode {
        "attached" => theme::OK,
        "paused" => theme::WARN,
        "degraded" | "safe_mode" => theme::ERR,
        _ => theme::MUTED,
    }
}

// --- worker thread -------------------------------------------------------

/// The single IPC thread (D11c): strictly serial, so replies arrive in
/// command order and the UI can keep at most one poll in flight.
fn worker(rx: Receiver<Cmd>, tx: Sender<Reply>) {
    while let Ok(cmd) = rx.recv() {
        let reply = match cmd {
            Cmd::Poll => {
                let status = ask_data::<Status>("status", no_data());
                // Not running: skip the two follow-up calls — each would
                // pay the full connect budget for the same answer.
                let down = matches!(status, Err(ClientError::NotRunning));
                let (library, settings) = if down {
                    (Err(ClientError::NotRunning), Err(ClientError::NotRunning))
                } else {
                    (
                        ask_data::<LibraryItemsData>("library.list", no_data()),
                        ask_data::<Settings>("settings.get", no_data()),
                    )
                };
                Reply::Snapshot {
                    status,
                    library,
                    settings,
                }
            }
            Cmd::Add(paths) => Reply::Added(ask_data("library.add", LibraryAddData { paths })),
            Cmd::Remove(id) => Reply::Simple(ask_ok("library.remove", LibraryRemoveData { id })),
            Cmd::Set(id) => Reply::SetDone(ask_data(
                "wallpaper.set",
                SetData {
                    path: None,
                    id: Some(id),
                    monitor: None,
                },
            )),
            Cmd::Resume => Reply::Simple(ask_ok("playback.resume", no_data())),
            Cmd::StartEngine => Reply::Started(start_engine()),
        };
        if tx.send(reply).is_err() {
            break; // the window is gone
        }
    }
}

/// One request/response over the pipe, decoded into the shared wire type.
/// A payload that no longer decodes means the two sides disagree on the
/// protocol — the same remedy as a version mismatch (ipc.md §7).
fn ask_data<T: serde::de::DeserializeOwned>(
    cmd: &str,
    data: impl serde::Serialize,
) -> Result<T, ClientError> {
    let value = serde_json::to_value(data).map_err(|_| ClientError::Io(0))?;
    serde_json::from_value(client::request(cmd, value)?).map_err(|_| ClientError::VersionMismatch)
}

/// [`ask_data`] for commands whose response the UI does not read.
fn ask_ok(cmd: &str, data: impl serde::Serialize) -> Result<(), ClientError> {
    ask_data::<serde_json::Value>(cmd, data).map(|_| ())
}

/// Start the engine through the same command the CLI uses (D11d):
/// `run --detached` refuses a second instance, spawns the daemon with
/// `DETACHED_PROCESS` + NUL stdio and returns once the pipe answers.
/// Runs on the worker thread — the UI never waits for it.
fn start_engine() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("could not locate this executable: {err}"))?;
    let output = std::process::Command::new(exe)
        .args(["run", "--detached"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|err| format!("could not start the engine: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stderr.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        stderr
    };
    let text = text.trim();
    if text.is_empty() {
        Err(format!("the engine exited with {}", output.status))
    } else {
        Err(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built through serde so only the wire-required fields are named —
    /// the same shape the daemon sends (library.rs defaults the rest).
    fn item(id: &str) -> LibraryItem {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "path": format!("C:\\\\videos\\\\{id}.mp4"),
            "title": id,
            "width": 1920,
            "height": 1080,
            "fps": 60.0,
            "duration_ms": 65_000,
            "codec": "H264",
            "added_at": 1_700_000_000
        }))
        .expect("test item shape matches LibraryItem")
    }

    #[test]
    fn duration_follows_the_cli_shape() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(65_000), "1:05");
        assert_eq!(format_duration(59_999), "0:59");
        // Past an hour the hours field appears (same as `library ls`).
        assert_eq!(format_duration(3_725_000), "1:02:05");
    }

    #[test]
    fn rejections_read_like_the_cli() {
        let rejected = vec![RejectedPath {
            path: "C:\\videos\\clip.txt".to_string(),
            reason: "not a video Media Foundation can open".to_string(),
        }];
        let text = format_rejections(&rejected);
        assert!(text.starts_with("Not added:"), "{text}");
        assert!(
            text.contains("C:\\videos\\clip.txt \u{2014} not a video"),
            "{text}"
        );
        assert_eq!(format_rejections(&[]), "Not added:");
    }

    #[test]
    fn selection_drops_ids_that_left_the_list() {
        let library = [item("aa11"), item("bb22")];
        let mut selected = Some("bb22".to_string());
        drop_missing_selection(&library, &mut selected);
        assert_eq!(selected.as_deref(), Some("bb22"));

        let mut selected = Some("dead".to_string());
        drop_missing_selection(&library, &mut selected);
        assert_eq!(selected, None);

        let mut selected = None;
        drop_missing_selection(&library, &mut selected);
        assert_eq!(selected, None);
    }

    #[test]
    fn ordinals_are_the_list_position_and_meta_is_complete() {
        let library = [item("aa11"), item("bb22")];
        // D1: the display number is the 1-based position, never the id.
        let first = format!("{}. {}", 1, library[0].title);
        assert_eq!(first, "1. aa11");
        let meta = item_meta(&library[0]);
        assert!(meta.contains("1920x1080"), "{meta}");
        assert!(meta.contains("H264"), "{meta}");
        assert!(meta.contains("1:05"), "{meta}");
    }

    #[test]
    fn strip_colours_map_documented_modes_only() {
        assert_eq!(mode_color("attached"), theme::OK);
        assert_eq!(mode_color("paused"), theme::WARN);
        assert_eq!(mode_color("degraded"), theme::ERR);
        assert_eq!(mode_color("safe_mode"), theme::ERR);
        assert_eq!(mode_color("boot"), theme::MUTED);
        assert_eq!(mode_color("something-new"), theme::MUTED);
    }

    #[test]
    fn palette_matches_the_design_table() {
        // design-m5 §5.1 — a silent hex drift would change the theme.
        assert_eq!(theme::BG, egui::Color32::from_rgb(0x0A, 0x0A, 0x0A));
        assert_eq!(theme::PANEL, egui::Color32::from_rgb(0x14, 0x14, 0x16));
        assert_eq!(theme::SURFACE, egui::Color32::from_rgb(0x1C, 0x1C, 0x20));
        assert_eq!(theme::BORDER, egui::Color32::from_rgb(0x2A, 0x2A, 0x2E));
        assert_eq!(theme::TEXT, egui::Color32::from_rgb(0xFA, 0xFA, 0xFA));
        assert_eq!(theme::MUTED, egui::Color32::from_rgb(0x9A, 0x9A, 0xA3));
        assert_eq!(theme::ACCENT, egui::Color32::from_rgb(0x5D, 0x5F, 0xEF));
        assert_eq!(
            theme::ACCENT_HOVER,
            egui::Color32::from_rgb(0x4F, 0x51, 0xD6)
        );
    }
}
