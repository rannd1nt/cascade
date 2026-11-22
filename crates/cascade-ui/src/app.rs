//! The library window: connection state, numbered library list, and the
//! status strip (design-m5 §5). Request traffic runs on one worker
//! thread (D11c) — the UI thread only sends commands and drains replies,
//! so a wedged or absent daemon can never freeze a frame. The persistent
//! event subscription runs on a second thread beside the worker
//! (design-m5-p4 §5): it never issues requests, it only pushes
//! `Reply::Event` into the same reply channel.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use cascade_core::client::{self, ClientError};
use cascade_core::ipc::{
    no_data, Event, LibraryAddData, LibraryAddResult, LibraryItemsData, LibraryRemoveData,
    RejectedPath, SetData, SetReport, Settings, Status,
};
use cascade_core::library::LibraryItem;
use eframe::egui::{self, vec2};

use crate::theme;
use crate::thumbs::{self, TextureLru, ThumbDone, ThumbReq};

/// Refresh cadence while the window draws (design-m5 §8: the strip polls
/// `status` every 1 s, never faster — AGENTS §13).
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// While the engine is down every poll pays the connect budget
/// (3 × 250 ms, `ipc.md` §7) inside the worker, so poll at half rate.
const POLL_INTERVAL_DISCONNECTED: Duration = Duration::from_secs(2);
/// Event-stream reconnect backoff (`ipc.md` §7: never spin — 500 ms
/// doubling to 5 s).
const EVENT_BACKOFF_MIN: Duration = Duration::from_millis(500);
const EVENT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Library grid geometry (design-m5-p4 §11.4): the on-screen tile is
/// 160×90 pt under a 320×180 texture (2× the tile, sharp at 100–200 %
/// DPI — AGENTS §1). The row adds the ordinal/title line (15 pt) and
/// the meta line (13 pt): 90 + 4 + 15 + 2 + 13 = 124.
const TILE_W: f32 = 160.0;
const THUMB_H: f32 = 90.0;
const ROW_H: f32 = 124.0;
/// Height reserved under the grid for the separator + action row: the
/// buttons sit **below** the grid (`design-m5.md` §5 sketch), so the
/// grid must never grow into their space.
const BOTTOM_H: f32 = 56.0;
/// Smallest grid the window still shows at its 640×400 floor.
const MIN_GRID_H: f32 = 90.0;

/// Commands UI → worker. The worker runs them strictly in order, so at
/// most one `Poll` is ever in flight (`poll_outstanding`) and replies
/// can never interleave.
enum Cmd {
    /// One status/settings snapshot; `library` is false while the event
    /// stream is live — the events own the list then (design-m5-p4 §5).
    Poll {
        library: bool,
    },
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
        /// `None` when the poll skipped `library.list` on purpose —
        /// the current list stays untouched.
        library: Option<Result<LibraryItemsData, ClientError>>,
        settings: Result<Settings, ClientError>,
    },
    Added(Result<LibraryAddResult, ClientError>),
    /// `library.remove` / `playback.resume`: success carries no data the
    /// UI needs — the next poll shows the effect.
    Simple(Result<(), ClientError>),
    SetDone(Result<SetReport, ClientError>),
    Started(Result<(), String>),
    /// One server-pushed event (`ipc.md` §6) from the reader thread.
    Event(Event),
    /// The reader connected (`true`) or lost the stream (`false`); the
    /// UI resyncs statelessly on `true` (`ipc.md` §7).
    EventsLive(bool),
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
    /// True while the reader thread holds a live event subscription:
    /// the polls then skip `library.list` (design-m5-p4 §5).
    events_live: bool,
    /// One `library.list` fetch is still owed after the stream
    /// (re)connects: events carry *changes*, never the initial list
    /// (`ipc.md §6`), so the first poll after `EventsLive(true)` must
    /// include the library even though the stream is live (§7
    /// stateless resync). Cleared when that fetch lands.
    library_resync: bool,
    /// Search box text: client-side filter over title + path (D6).
    search: String,
    /// Thumb requests (UI → worker); `None` when no thread runs — the
    /// spawn failed, or the worker exited (T6).
    thumb_tx: Option<Sender<ThumbReq>>,
    /// Thumb results (worker → UI), drained once per `tick` (T7).
    thumb_rx: Receiver<ThumbDone>,
    /// Ids with a decode in flight — one request at a time per id (T6).
    thumb_pending: HashSet<String>,
    /// Ids whose decode failed → the reason shown on hover over the
    /// placeholder (T8). Cleared when the id set changes or the library
    /// is edited (T9); never a banner (§11.1).
    thumb_failed: HashMap<String, String>,
    /// GPU textures for the grid, LRU-limited (T10).
    textures: TextureLru<egui::TextureHandle>,
    /// The orphan prune already ran after the first successful list
    /// reply (T11) — never from an error path.
    pruned: bool,
}

impl CascadeApp {
    pub fn new(ctx: egui::Context) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
        let (thumb_tx, thumb_req_rx) = mpsc::channel::<ThumbReq>();
        let (thumb_done_tx, thumb_done_rx) = mpsc::channel::<ThumbDone>();
        let worker_tx = reply_tx.clone();
        std::thread::Builder::new()
            .name("cascade-ui-ipc".to_string())
            .spawn(move || worker(cmd_rx, worker_tx))
            .map_err(|err| format!("could not start the UI connection thread: {err}"))?;
        let mut app = Self::build(cmd_tx, reply_rx, Some(thumb_tx), thumb_done_rx);
        // The thumbnail thread is an enhancement, not the lifeline: if
        // it cannot start, every tile keeps its placeholder (§11.1) and
        // the window opens anyway.
        let thumb_ctx = ctx.clone();
        if std::thread::Builder::new()
            .name("cascade-ui-thumbs".to_string())
            .spawn(move || thumbs::thumb_worker(thumb_req_rx, thumb_done_tx, thumb_ctx))
            .is_err()
        {
            app.thumb_tx = None;
            app.notice =
                Some("thumbnails unavailable \u{2014} the grid shows placeholders".to_string());
        }
        // The event stream is an enhancement, not the lifeline: if this
        // thread cannot start the window degrades to full polling with
        // an honest notice instead of failing to open.
        if std::thread::Builder::new()
            .name("cascade-ui-events".to_string())
            .spawn(move || event_reader(reply_tx, ctx))
            .is_err()
        {
            app.notice = Some(
                "live updates unavailable \u{2014} the window refreshes once per second"
                    .to_string(),
            );
        }
        Ok(app)
    }

    fn build(
        cmd: Sender<Cmd>,
        reply: Receiver<Reply>,
        thumb_tx: Option<Sender<ThumbReq>>,
        thumb_rx: Receiver<ThumbDone>,
    ) -> Self {
        Self {
            cmd,
            reply,
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
            events_live: false,
            library_resync: false,
            search: String::new(),
            thumb_tx,
            thumb_rx,
            thumb_pending: HashSet::new(),
            thumb_failed: HashMap::new(),
            textures: TextureLru::new(thumbs::LRU_LIMIT),
            pruned: false,
        }
    }

    /// Test constructor: identical state, no threads — the worker would
    /// need a daemon, and the reader would subscribe to a live one.
    /// `thumb_tx: None` says the same for the thumbnail worker.
    #[cfg(test)]
    fn detached() -> Self {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
        drop(reply_tx); // no producer: tests drive `apply` directly
        let (_done_tx, done_rx) = mpsc::channel::<ThumbDone>();
        Self::build(cmd_tx, reply_rx, None, done_rx)
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
        // After the replies: results are validated against the freshest
        // list, so a removal that raced a decode drops its texture (T9).
        self.drain_thumbs(ctx);
        if !self.poll_outstanding && Instant::now() >= self.poll_at {
            let library = self.poll_carries_library();
            if self.cmd.send(Cmd::Poll { library }).is_ok() {
                self.poll_outstanding = true;
            } else {
                self.mark_worker_dead();
            }
        }
        // Reschedule every frame; a hidden window draws no frame, so no
        // repaint is requested and no poll is sent (architecture §9).
        ctx.request_repaint_after(POLL_INTERVAL);
    }

    /// Drain finished thumbnails into GPU textures (T7): `load_texture`
    /// runs only here, on the UI thread — never on the worker.
    fn drain_thumbs(&mut self, ctx: &egui::Context) {
        loop {
            match self.thumb_rx.try_recv() {
                Ok(done) => self.apply_thumb(ctx, done),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The worker exited (it never started, or it died):
                    // every tile keeps its placeholder from now on — no
                    // retry loops (§11.1).
                    self.thumb_tx = None;
                    break;
                }
            }
        }
    }

    /// One result: forget its pending request, then either build the
    /// texture or remember the failure reason (T7/T8). A result for an
    /// id that left the library is dropped (T9).
    fn apply_thumb(&mut self, ctx: &egui::Context, done: ThumbDone) {
        let (id, result) = match done {
            ThumbDone::Ready {
                id,
                width,
                height,
                rgba,
            } => (id, Ok((width, height, rgba))),
            ThumbDone::Failed { id, reason } => (id, Err(reason)),
        };
        self.thumb_pending.remove(&id);
        if !self.library.iter().any(|item| item.id == id) {
            return;
        }
        match result {
            Ok((width, height, rgba)) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [width as usize, height as usize],
                    &rgba,
                );
                let handle = ctx.load_texture(
                    format!("cascade-thumb-{id}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(id, handle);
            }
            Err(reason) => {
                self.thumb_failed.insert(id, reason);
            }
        }
    }

    /// Adopt a new library list (T9): textures and in-flight requests
    /// for ids that stayed, everything dropped for ids that left, and
    /// all failure reasons cleared when the id set changed — a fresh id
    /// must get a fresh attempt. `prune` is true **only** from a
    /// successful `library.list` reply: the single place an orphan
    /// prune may run (T11 — an empty guess from an error path would
    /// wipe a valid cache).
    fn set_library(&mut self, items: Vec<LibraryItem>, prune: bool) {
        let old: HashSet<String> = self.library.iter().map(|item| item.id.clone()).collect();
        let new: HashSet<String> = items.iter().map(|item| item.id.clone()).collect();
        let changed = old != new;
        self.library = items;
        drop_missing_selection(&self.library, &mut self.selected);
        self.textures.retain_ids(&new);
        self.thumb_pending.retain(|id| new.contains(id));
        if changed {
            self.thumb_failed.clear();
        }
        if prune && !self.pruned {
            self.pruned = true;
            // The prune compiles only outside tests (`cfg(not(test))`,
            // mirrored here): a unit test never touches the real cache
            // directory (AGENTS §7). The full prune is V4 on target.
            #[cfg(not(test))]
            {
                let ids: Vec<String> = new.into_iter().collect();
                let removed = thumbs::prune_orphans(&ids);
                if removed > 0 {
                    eprintln!("cascade: ui: pruned {removed} orphan thumbnail cache file(s)");
                }
            }
        }
    }

    /// Poll again as soon as possible (after an action, or on Retry).
    fn request_poll_now(&mut self) {
        self.poll_at = Instant::now();
    }

    /// Whether the next poll must carry `library.list`: always while no
    /// subscription is live, plus the one owed resync fetch after a
    /// (re)connect (design-m5-p4 §5, `ipc.md §7`).
    fn poll_carries_library(&self) -> bool {
        !self.events_live || self.library_resync
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
                        // so no stale list, strip values, or thumbnail
                        // textures linger.
                        self.conn = Conn::NotRunning;
                        self.status = None;
                        self.settings = None;
                        self.set_library(Vec::new(), false);
                        self.last_snapshot_error = None;
                    }
                    Err(err) => {
                        self.conn = Conn::Unreachable(err.to_string());
                        self.status = None;
                    }
                }
                if self.conn == Conn::Connected {
                    match library {
                        Some(Ok(data)) => {
                            // A carried `library.list` discharges the
                            // resync owed after (re)connecting.
                            self.library_resync = false;
                            // The one path where an orphan prune is
                            // allowed (T11): a *successful* list reply.
                            self.set_library(data.items, true);
                            self.last_snapshot_error = None;
                        }
                        Some(Err(err)) => {
                            self.library_resync = false;
                            self.snapshot_error(err.to_string());
                        }
                        // Skipped only while the event stream is live
                        // *and* no resync is owed — the events own the
                        // list from then on (design-m5-p4 §5).
                        None => {}
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
            Reply::EventsLive(live) => {
                let became_live = live && !self.events_live;
                self.events_live = live;
                if became_live {
                    // Events are not replayed (`ipc.md` §6): resync with
                    // one full poll right after (re)connecting (§7) —
                    // full means it *carries* `library.list`, which
                    // `library_resync` forces even while live. Without
                    // this the window could start empty and stay empty
                    // until the next `library.changed` (seen on the
                    // target machine, P4-S2: a UI started after the
                    // daemon showed 0 videos).
                    self.library_resync = true;
                    self.request_poll_now();
                }
                // Going false needs no action: the next scheduled poll
                // carries `library.list` again within one interval.
            }
            Reply::Event(event) => self.apply_event(event),
        }
    }

    /// One pushed event (`ipc.md` §6). The reader thread delivers them
    /// in order; anything the UI cannot use falls back to a poll rather
    /// than trusting a payload it does not understand.
    fn apply_event(&mut self, event: Event) {
        match event.event.as_str() {
            // Mode/pause/applied-video changed: refresh the strip. The
            // scheduled poll would do it within 1 s anyway — this makes
            // it immediate (design-m5-p4 §5).
            "state" => self.request_poll_now(),
            "library.changed" => match serde_json::from_value::<LibraryItemsData>(event.data) {
                Ok(data) => {
                    self.set_library(data.items, false);
                    // The list was *edited*: give every failed thumbnail
                    // one fresh attempt (T8) — a mutation is the only
                    // thing that can change a file's state behind our
                    // back.
                    self.thumb_failed.clear();
                }
                // Payload disagreement → the same remedy as a version
                // mismatch: refetch, do not guess (ipc.md §7).
                Err(_) => self.request_poll_now(),
            },
            "log" => {
                if let Some(message) = event
                    .data
                    .get("message")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                {
                    // Surface once: do not repeat the notice that is
                    // already on screen.
                    if self.notice.as_deref() != Some(message.as_str()) {
                        self.notice = Some(message);
                    }
                }
            }
            // The daemon is going away (or restarting after a set):
            // ask now — the answer decides NotRunning vs the new PID.
            "shutdown" => self.request_poll_now(),
            // Forward compatible: an event from a newer daemon the UI
            // does not know is ignored, never fatal (ipc.md §2).
            _ => {}
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

        // Search **above** the grid (`design-m5.md` §5 sketch): a
        // client-side filter over title + path (D6).
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 32.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search title or path\u{2026}")
                    .desired_width(width),
            );
            if !self.search.is_empty()
                && ui
                    .small_button("\u{d7}")
                    .on_hover_text("Clear search")
                    .clicked()
            {
                self.search.clear();
            }
        });
        ui.add_space(8.0);

        if self.library.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label("No videos yet.");
                ui.add_space(6.0);
                theme::muted(
                    ui,
                    "Files are referenced, never copied \u{2014} use + Add below, or drop files onto this window.",
                );
            });
            ui.add_space(28.0);
        } else {
            self.library_grid(ui);
        }

        // The actions live **below** the grid (`design-m5.md` §5). They
        // are drawn last on purpose: a ScrollArea can only grow down, so
        // nothing the list does can push them off-panel (the P2b lesson,
        // now on the other side of the grid).
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);
        self.action_row(ui);
    }

    /// The virtualized tile grid (design-m5-p4 §11.4): `show_rows`
    /// renders only the visible rows, each visible tile requests its
    /// thumbnail on render (T6), and every action is collected during
    /// the pass and applied after it (edition 2021 disjoint captures).
    fn library_grid(&mut self, ui: &mut egui::Ui) {
        let filtered: Vec<usize> = (0..self.library.len())
            .filter(|&index| matches_query(&self.library[index], &self.search))
            .collect();
        if filtered.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label("No matches.");
                ui.add_space(6.0);
                theme::muted(
                    ui,
                    &format!(
                        "Nothing in the library matches \u{201c}{}\u{201d}.",
                        self.search.trim()
                    ),
                );
            });
            ui.add_space(28.0);
            return;
        }

        // Columns follow the live width — no monitor assumption
        // (AGENTS §1). The horizontal gap is the current item spacing
        // (8 pt default); `show_rows` adds the same spacing vertically,
        // so the pitch matches the row advance exactly.
        let spacing = ui.spacing().item_spacing;
        let columns =
            (((ui.available_width() + spacing.x) / (TILE_W + spacing.x)).floor() as usize).max(1);
        let total_rows = filtered.len().div_ceil(columns);
        // Reserve the action row's height under the grid (BOTTOM_H).
        let grid_h = (ui.available_height() - BOTTOM_H).max(MIN_GRID_H);

        let mut enqueue: Vec<ThumbReq> = Vec::new();
        let mut select: Option<String> = None;
        let mut set_row: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(grid_h)
            .show_rows(ui, ROW_H, total_rows, |ui, row_range| {
                for row_index in row_range {
                    ui.horizontal(|ui| {
                        for column in 0..columns {
                            let index = row_index * columns + column;
                            let Some(&item_index) = filtered.get(index) else {
                                break;
                            };
                            let item = &self.library[item_index];
                            // Request-on-render (T6): only visible tiles
                            // ask, each id asks once, and a failure is
                            // never re-asked until the list changes
                            // (T8/T9) — no retry loops (§11.1).
                            if self.thumb_tx.is_some()
                                && !self.textures.contains(&item.id)
                                && !self.thumb_pending.contains(&item.id)
                                && !self.thumb_failed.contains_key(&item.id)
                            {
                                self.thumb_pending.insert(item.id.clone());
                                enqueue.push(ThumbReq {
                                    id: item.id.clone(),
                                    path: item.path.clone(),
                                });
                            }
                            let texture = self.textures.get_touch(&item.id);
                            let failure = self.thumb_failed.get(&item.id).map(String::as_str);
                            let meta = item_meta(item);
                            let response = ui.add(LibraryTile {
                                // Ordinals renumber over the *filtered*
                                // list (search keeps 1..n contiguous).
                                ordinal: index + 1,
                                title: &item.title,
                                meta: &meta,
                                path: &item.path,
                                selected: self.selected.as_deref() == Some(item.id.as_str()),
                                texture,
                                failure,
                            });
                            if response.clicked() {
                                select = Some(item.id.clone());
                            }
                            if response.double_clicked() {
                                set_row = Some(item.id.clone());
                            }
                        }
                    });
                }
            });
        if let Some(id) = select {
            self.selected = Some(id);
        }
        if let Some(id) = set_row {
            self.send(Cmd::Set(id));
        }
        // Hand the collected requests to the thumb thread (T6). The
        // worker wakes the UI itself when a result arrives.
        if let Some(tx) = self.thumb_tx.as_ref() {
            for req in enqueue {
                let _ = tx.send(req);
            }
        }
    }

    /// The three actions under the grid: Add / Remove / Set as
    /// wallpaper — unchanged semantics (`design-m5.md` §5).
    fn action_row(&mut self, ui: &mut egui::Ui) {
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

/// Client-side search (D6): case-insensitive substring over title and
/// path. An empty or whitespace-only query matches everything — pure,
/// unit-tested (design-m5-p4 §11.5 V5).
fn matches_query(item: &LibraryItem, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    item.title.to_lowercase().contains(&needle) || item.path.to_lowercase().contains(&needle)
}

/// One grid tile: thumbnail + ordinal/title line + meta line inside a
/// single 160×124 allocation (design-m5-p4 §11.4). Painted by hand so
/// the row's cursor math stays exactly one `allocate_exact_size`.
struct LibraryTile<'a> {
    /// 1-based position in the *filtered* list (renumbers with search).
    ordinal: usize,
    title: &'a str,
    meta: &'a str,
    path: &'a str,
    selected: bool,
    /// `Some` = decoded texture; `None` = placeholder (decoding/failed).
    texture: Option<&'a egui::TextureHandle>,
    /// Failure reason for the hover; `None` while still decoding.
    failure: Option<&'a str>,
}

impl egui::Widget for LibraryTile<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(vec2(TILE_W, ROW_H), egui::Sense::click());
        let thumb = egui::Rect::from_min_size(rect.min, vec2(TILE_W, THUMB_H));
        let painter = ui.painter();
        match self.texture {
            Some(handle) => {
                painter.image(
                    handle.id(),
                    thumb,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                // Placeholder: the same surface the letterbox pad uses,
                // so a decoding tile and a letterboxed frame blend.
                painter.rect_filled(thumb, 2.0, theme::SURFACE);
                if self.failure.is_some() {
                    painter.text(
                        thumb.center(),
                        egui::Align2::CENTER_CENTER,
                        "no preview",
                        egui::FontId::proportional(12.0),
                        theme::MUTED,
                    );
                }
            }
        }
        let stroke = if self.selected {
            egui::Stroke::new(2.0, theme::ACCENT)
        } else {
            egui::Stroke::new(1.0, theme::BORDER)
        };
        painter.rect(
            thumb,
            2.0,
            egui::Color32::TRANSPARENT,
            stroke,
            egui::StrokeKind::Inside,
        );

        // The two text lines live in their own scope: `Ui::put` is a
        // `scope_builder` that advances the *calling* ui's cursor to the
        // widget rect (verified, egui 0.37 `ui.rs:1691`) — without this
        // scope the puts would pull the row's cursor back under the
        // tile. Both rects span the full tile width and the meta line
        // ends exactly at the tile bottom, so the scope's advance
        // lands on the same point as the allocation above.
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            let title_rect =
                egui::Rect::from_min_size(rect.min + vec2(0.0, THUMB_H + 4.0), vec2(TILE_W, 15.0));
            ui.put(
                title_rect,
                egui::Label::new(format!("{}. {}", self.ordinal, self.title))
                    .truncate()
                    .halign(egui::Align::Min),
            );
            let meta_rect =
                egui::Rect::from_min_size(rect.min + vec2(0.0, THUMB_H + 21.0), vec2(TILE_W, 13.0));
            ui.put(
                meta_rect,
                egui::Label::new(egui::RichText::new(self.meta).color(theme::MUTED))
                    .truncate()
                    .halign(egui::Align::Min),
            );
        });

        // Hover: the path always; a failed decode adds its reason
        // (T8) — the placeholder explains itself, never a banner.
        match self.failure {
            Some(reason) => response.on_hover_text(format!("{}\n{}", self.path, reason)),
            None => response.on_hover_text(self.path),
        }
    }
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
            Cmd::Poll { library } => {
                let status = ask_data::<Status>("status", no_data());
                // Not running: skip the two follow-up calls — each would
                // pay the full connect budget for the same answer.
                let down = matches!(status, Err(ClientError::NotRunning));
                let (library, settings) = if down {
                    (None, Err(ClientError::NotRunning))
                } else if library {
                    (
                        Some(ask_data::<LibraryItemsData>("library.list", no_data())),
                        ask_data::<Settings>("settings.get", no_data()),
                    )
                } else {
                    // The event stream is live and owns the list
                    // (design-m5-p4 §5): status + settings only.
                    (None, ask_data::<Settings>("settings.get", no_data()))
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

/// The event subscription thread (design-m5-p4 §5): holds one persistent
/// connection to `cascade-evt-{session}`, forwards every frame as
/// [`Reply::Event`], and reports connectivity as [`Reply::EventsLive`].
/// It never issues a request — the worker keeps its serial command order
/// (D11c). Reconnects follow `ipc.md` §7: 500 ms doubling to 5 s, never
/// a spin; a fresh connection means a stateless resync on the UI side.
///
/// `ctx` is woken after every delivery: egui draws only when something
/// asks for a frame, and the idle schedule is one frame per second —
/// without the wake an event waits up to `POLL_INTERVAL` before it is
/// even read (measured 2026-11-21: a library row appeared 0–1000 ms
/// late, design-m5-p4 §8 V1).
fn event_reader(tx: Sender<Reply>, ctx: egui::Context) {
    let mut backoff = EVENT_BACKOFF_MIN;
    loop {
        if let Ok(mut stream) = client::EventStream::open() {
            backoff = EVENT_BACKOFF_MIN;
            if tx.send(Reply::EventsLive(true)).is_err() {
                return; // the window is gone
            }
            ctx.request_repaint();
            // Ends on the first error: daemon exited, pipe broken,
            // or a payload we refuse — reconnect below.
            while let Ok(event) = stream.read_event() {
                if tx.send(Reply::Event(event)).is_err() {
                    return; // the window is gone
                }
                ctx.request_repaint();
            }
        }
        // No daemon (yet) or the one subscriber slot is taken: back off
        // and try again — never a spin (`ipc.md` §7).
        if tx.send(Reply::EventsLive(false)).is_err() {
            return; // the window is gone
        }
        ctx.request_repaint();
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(EVENT_BACKOFF_MAX);
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

    // --- design-m5-p4 §5: event consumption ------------------------------

    use cascade_core::ipc::{AutostartStatus, HostStatus, MemoryStatus};
    use cascade_core::PROTOCOL_VERSION;

    fn status_fixture() -> Status {
        Status {
            engine: "running".to_string(),
            mode: "attached".to_string(),
            paused_reasons: Vec::new(),
            monitors: Vec::new(),
            memory: MemoryStatus {
                working_set_mb: 100,
                private_mb: 50,
            },
            host: HostStatus {
                strategy: "workerw_child".to_string(),
                repaint_level: 1,
            },
            autostart: AutostartStatus {
                enabled: true,
                windows_disabled: false,
            },
        }
    }

    fn snapshot(library: Option<Result<LibraryItemsData, ClientError>>) -> Reply {
        Reply::Snapshot {
            status: Ok(status_fixture()),
            library,
            settings: Ok(Settings::default()),
        }
    }

    fn event(name: &str, data: serde_json::Value) -> Event {
        Event {
            v: PROTOCOL_VERSION,
            event: name.to_string(),
            data,
        }
    }

    fn items_data(items: Vec<LibraryItem>) -> Result<LibraryItemsData, ClientError> {
        let items = serde_json::to_value(items).expect("test items encode");
        let data = serde_json::from_value(serde_json::json!({ "items": items }))
            .expect("test items decode into the wire shape");
        Ok(data)
    }

    #[test]
    fn library_changed_event_replaces_the_list_and_drops_dead_selection() {
        let mut app = CascadeApp::detached();
        app.library = vec![item("aa11"), item("bb22")];
        app.selected = Some("bb22".to_string());

        // The new list keeps aa11 only: bb22's selection must go with it.
        let data = serde_json::to_value(vec![item("aa11")]).expect("items encode");
        app.apply(Reply::Event(event(
            "library.changed",
            serde_json::json!({ "items": data }),
        )));
        assert_eq!(app.library.len(), 1, "the list is replaced wholesale");
        assert_eq!(app.library[0].id, "aa11");
        assert_eq!(app.selected, None, "a vanished id never stays selected");

        // A list that still contains the selected id keeps it.
        app.selected = Some("aa11".to_string());
        let data = serde_json::to_value(vec![item("aa11")]).expect("items encode");
        app.apply(Reply::Event(event(
            "library.changed",
            serde_json::json!({ "items": data }),
        )));
        assert_eq!(app.selected.as_deref(), Some("aa11"));
    }

    #[test]
    fn state_and_shutdown_poll_now_and_unknown_events_are_ignored() {
        let mut app = CascadeApp::detached();
        app.apply(snapshot(None));
        let scheduled = app.poll_at;
        assert!(scheduled > Instant::now(), "a settled poll sits 1 s out");

        app.apply(Reply::Event(event(
            "state",
            serde_json::json!({"mode": "paused", "paused_reasons": ["manual"]}),
        )));
        assert!(
            app.poll_at <= Instant::now(),
            "a state event refreshes the strip immediately"
        );

        app.apply(snapshot(None));
        let scheduled = app.poll_at;
        app.apply(Reply::Event(event("future.thing", serde_json::json!({}))));
        assert_eq!(
            app.poll_at, scheduled,
            "an unknown event from a newer daemon is ignored, never fatal"
        );

        app.apply(Reply::Event(event("shutdown", serde_json::json!({}))));
        assert!(
            app.poll_at <= Instant::now(),
            "shutdown asks the poll to decide NotRunning vs the new pid"
        );
    }

    #[test]
    fn events_live_resyncs_once_and_reports_disconnects() {
        let mut app = CascadeApp::detached();
        app.apply(snapshot(None));
        assert!(!app.events_live, "starts without a subscription");

        app.apply(Reply::EventsLive(true));
        assert!(app.events_live);
        assert!(
            app.poll_at <= Instant::now(),
            "events are not replayed: one stateless resync (ipc.md §7)"
        );

        // A duplicate `true` must not queue extra work.
        app.apply(snapshot(None));
        let scheduled = app.poll_at;
        app.apply(Reply::EventsLive(true));
        assert_eq!(app.poll_at, scheduled, "resync happens once per connect");

        app.apply(Reply::EventsLive(false));
        assert!(!app.events_live, "the next poll carries library again");
    }

    #[test]
    fn resync_after_connect_carries_the_initial_library() {
        let mut app = CascadeApp::detached();
        // The daemon already has items when this window opens; events
        // carry changes, never the initial list (ipc.md §6).
        assert!(
            app.poll_carries_library(),
            "the first poll always fetches the library"
        );

        // If the event thread wins the race before that poll is sent,
        // the resync still has to carry library.list (P4-S2: the UI
        // opened with 0 videos while the daemon had 17).
        app.apply(Reply::EventsLive(true));
        assert!(
            app.poll_carries_library(),
            "a live stream owes exactly one full poll"
        );

        app.apply(snapshot(Some(items_data(vec![item("aa11")]))));
        assert_eq!(app.library.len(), 1, "the resync fills the grid");
        assert!(
            !app.poll_carries_library(),
            "after the resync the events own the list"
        );
        // A duplicate `true` must not owe a second library fetch.
        app.apply(Reply::EventsLive(true));
        assert!(!app.poll_carries_library());
    }

    #[test]
    fn log_events_surface_once_and_never_stack() {
        let mut app = CascadeApp::detached();
        app.apply(Reply::Event(event(
            "log",
            serde_json::json!({"level": "warn", "message": "could not load clip.mp4"}),
        )));
        assert_eq!(app.notice.as_deref(), Some("could not load clip.mp4"));

        // The same warning again must not disturb the visible notice.
        app.apply(Reply::Event(event(
            "log",
            serde_json::json!({"level": "warn", "message": "could not load clip.mp4"}),
        )));
        assert_eq!(app.notice.as_deref(), Some("could not load clip.mp4"));

        app.apply(Reply::Event(event(
            "log",
            serde_json::json!({"level": "warn", "message": "no media backend is available"}),
        )));
        assert_eq!(app.notice.as_deref(), Some("no media backend is available"));
    }

    #[test]
    fn a_gated_poll_keeps_the_event_owned_list_until_the_engine_is_down() {
        let mut app = CascadeApp::detached();
        app.apply(snapshot(Some(items_data(vec![item("aa11")]))));
        assert_eq!(app.library.len(), 1);

        // Live subscription: the poll skips library.list on purpose.
        app.events_live = true;
        app.apply(snapshot(None));
        assert_eq!(
            app.library.len(),
            1,
            "a gated poll never touches the event-owned list"
        );

        // NotRunning clears everything regardless of the gate: no stale
        // list may linger behind a dead daemon.
        app.apply(Reply::Snapshot {
            status: Err(ClientError::NotRunning),
            library: None,
            settings: Err(ClientError::NotRunning),
        });
        assert!(app.library.is_empty());
        assert_eq!(app.selected, None);
    }

    // --- design-m5-p4 §11: search + thumbnail state ---------------------

    #[test]
    fn search_matches_title_and_path_case_insensitively() {
        let mut clip = item("aa11");
        clip.title = "Furina Midnight".to_string();
        assert!(matches_query(&clip, ""), "an empty query matches all");
        assert!(matches_query(&clip, "   "), "a blank query matches all");
        assert!(matches_query(&clip, "furina"), "title, lower-case needle");
        assert!(matches_query(&clip, "FURINA"), "title, upper-case needle");
        assert!(matches_query(&clip, "videos"), "path substring");
        assert!(!matches_query(&clip, "ganyu"), "no false match");
    }

    #[test]
    fn adopting_a_new_list_syncs_the_thumbnail_state() {
        let mut app = CascadeApp::detached();
        app.set_library(vec![item("aa11"), item("bb22")], false);
        app.thumb_failed
            .insert("bb22".to_string(), "codec".to_string());
        app.thumb_pending.insert("bb22".to_string());

        // Same id set: a refresh must not wipe the failure reasons (a
        // poll repeats the same list every second).
        app.set_library(vec![item("aa11"), item("bb22")], false);
        assert!(app.thumb_failed.contains_key("bb22"));
        assert!(app.thumb_pending.contains("bb22"));

        // Id set changed: failures clear and requests for gone ids are
        // dropped — a fresh id gets exactly one fresh attempt (T9).
        app.set_library(vec![item("aa11"), item("cc33")], false);
        assert!(app.thumb_failed.is_empty());
        assert!(!app.thumb_pending.contains("bb22"));
    }

    #[test]
    fn engine_down_drops_the_thumbnail_state_too() {
        let mut app = CascadeApp::detached();
        app.set_library(vec![item("aa11")], false);
        app.thumb_failed
            .insert("aa11".to_string(), "boom".to_string());
        app.apply(Reply::Snapshot {
            status: Err(ClientError::NotRunning),
            library: None,
            settings: Err(ClientError::NotRunning),
        });
        assert!(app.library.is_empty());
        assert!(
            app.thumb_failed.is_empty(),
            "a dead engine leaves no stale thumbnail state"
        );
    }
}
