//! The library window: connection state, numbered library grid, the
//! Settings surface. Request traffic
//! runs on one worker thread — the UI thread only sends commands
//! and drains replies, so a wedged or absent daemon can never freeze a
//! frame. The persistent event subscription runs on a second thread beside
//! the worker: it never issues requests, it only pushes
//! `Reply::Event` into the same reply channel.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use cascade_core::client::{self, ClientError};
use cascade_core::ipc::{
    no_data, AutostartStatus, EnabledData, Event, LibraryAddData, LibraryAddResult,
    LibraryItemsData, LibraryRemoveData, PausePatch, RejectedPath, SetData, SetReport, Settings,
    SettingsPatch, Status,
};
use cascade_core::library::LibraryItem;
use cascade_core::rule_engine::{BatteryAction, PauseSettings, PowerProfile};
use cascade_core::{PresentationMode, Rect};
use eframe::egui::{self, vec2};

use crate::theme;
use crate::thumbs::{self, TextureLru, ThumbDone, ThumbReq};

/// Refresh cadence while the window draws : status/settings
/// are polled every 1 s, never faster — ).
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// While the engine is down every poll pays the connect budget
/// (3 × 250 ms) inside the worker, so poll at half rate.
const POLL_INTERVAL_DISCONNECTED: Duration = Duration::from_secs(2);
/// Event-stream reconnect backoff : never spin — 500 ms
/// doubling to 5 s).
const EVENT_BACKOFF_MIN: Duration = Duration::from_millis(500);
const EVENT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Library grid geometry (UI refinement): a 256×144 pt thumbnail
/// (16:9), then a wrapping title and a wrapping metadata line laid out
/// naturally so the gap between them is only `TEXT_GAP`. The cached
/// texture stays 320×180; the media/cache path is untouched here.
const TILE_W: f32 = 256.0;
const THUMB_H: f32 = 144.0;
/// Room for two wrapped lines of title + two of meta plus the small gap.
const TEXT_H: f32 = 72.0;
const TEXT_GAP: f32 = 4.0;
const ROW_H: f32 = THUMB_H + 6.0 + TEXT_H; // 222
/// Smallest grid the window still shows at its 640×400 floor.
const MIN_GRID_H: f32 = 90.0;
/// Action row: a compact left-aligned Add + icon-only search toggle, and
/// a revealed search field immediately beside the toggle at the same box height.
const SEARCH_W: f32 = 240.0;
const ADD_W: f32 = 104.0;
const ACTION_H: f32 = 30.0;
const INFO_SIZE: f32 = 22.0;
const SIDEBAR_ITEM_H: f32 = 44.0;

/// Commands UI → worker. The worker runs them strictly in order, so at
/// most one `Poll` is ever in flight (`poll_outstanding`) and replies
/// can never interleave.
enum Cmd {
    /// One status/settings snapshot; `library` is false while the event
    /// stream is live — the events own the list then.
    Poll {
        library: bool,
    },
    Add(Vec<String>),
    Remove(String),
    /// Library id plus the UI-local target monitor (`None` = daemon default).
    /// The monitor travels in the existing `SetData.monitor` field.
    Set(String, Option<String>),
    StartEngine,
    /// One settings write. The daemon answers with the full `Settings`
    /// payload; an engine-mode change additionally respawns *after* the
    /// answer.
    SettingsSet(SettingsPatch),
    /// Autostart is its own wire command — the Run key is the oracle, not
    /// `config.toml`.
    AutostartSet(bool),
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
    /// A `settings.set` answer — the new wire snapshot, not an echo we
    /// invent locally.
    SettingsChanged(Result<Settings, ClientError>),
    /// An `autostart.set` answer — the Run-key oracle after the write.
    AutostartChanged(Result<AutostartStatus, ClientError>),
    /// One server-pushed event from the reader thread.
    Event(Event),
    /// The reader connected (`true`) or lost the stream (`false`); the
    /// UI resyncs statelessly on `true`.
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

/// The surfaces implemented so far. Monitors is only shown
/// when >1 monitor exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Library,
    Settings,
    Monitors,
}

impl Tab {
    /// A display change can take the second monitor away while the surface
    /// is open. Fall back to Library rather than leaving a dead tab.
    fn normalize(self, monitor_count: usize) -> Self {
        if self == Self::Monitors && monitor_count <= 1 {
            Self::Library
        } else {
            self
        }
    }
}

/// Editable copy of the wire settings. `volume` is the live slider value —
/// there is no second "committed" copy: keeping one made a fast drag (or a
/// scroll made while an earlier write was in flight) snap back when the
/// snapshot landed (corrected 2026-11-22).
///
/// The whole form is protected by `CascadeApp::dirty`: while any draft is
/// unsent, the 1 s poll may not adopt a newer wire snapshot over it. That
/// is what keeps the engine-mode selection alive long enough for the
/// explicit "Apply & restart" button to appear.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SettingsForm {
    volume: u8,
    engine_mode: PresentationMode,
    power_profile: PowerProfile,
    on_battery: BatteryAction,
    pause: PauseSettings,
    restore_wallpaper_on_exit: bool,
    tray: bool,
    autostart: bool,
}

impl SettingsForm {
    fn from_wire(settings: &Settings, autostart: bool) -> Self {
        Self {
            volume: settings.volume,
            engine_mode: settings.engine_mode,
            power_profile: settings.power_profile,
            on_battery: settings.on_battery,
            pause: settings.pause,
            restore_wallpaper_on_exit: settings.restore_wallpaper_on_exit,
            tray: settings.tray,
            autostart,
        }
    }

    fn sync_from_wire(&mut self, settings: &Settings, autostart: bool) {
        *self = Self::from_wire(settings, autostart);
    }

    /// Pure diff against the last wire snapshot. `autostart` is excluded on
    /// purpose: it travels through `autostart.set`.
    /// `engine_mode` is excluded too — it needs the explicit confirmation
    /// button, never a silent change from a combo misclick.
    fn changes(
        &self,
        base: &Settings,
        autostart_now: bool,
        send_volume: bool,
    ) -> (Option<SettingsPatch>, Option<bool>) {
        let mut patch = SettingsPatch::default();
        let mut any = false;
        // A mid-drag slider is still a draft, but sending on every frame
        // would spam the worker. The caller gates this on drag release.
        if send_volume && self.volume != base.volume {
            patch.volume = Some(self.volume);
            any = true;
        }
        if self.power_profile != base.power_profile {
            patch.power_profile = Some(self.power_profile);
            any = true;
        }
        if self.on_battery != base.on_battery {
            patch.on_battery = Some(self.on_battery);
            any = true;
        }
        if self.pause != base.pause {
            let pause = self.pause;
            patch.pause = Some(PausePatch {
                fullscreen: Some(pause.fullscreen),
                maximized: Some(pause.maximized),
                display_off: Some(pause.display_off),
                session_lock: Some(pause.session_lock),
                screensaver: Some(pause.screensaver),
                remote_session: Some(pause.remote_session),
            });
            any = true;
        }
        if self.restore_wallpaper_on_exit != base.restore_wallpaper_on_exit {
            patch.restore_wallpaper_on_exit = Some(self.restore_wallpaper_on_exit);
            any = true;
        }
        if self.tray != base.tray {
            patch.tray = Some(self.tray);
            any = true;
        }
        let autostart = (self.autostart != autostart_now).then_some(self.autostart);
        (any.then_some(patch), autostart)
    }

    /// The explicit engine-mode write, kept pure so the confirmation path
    /// is unit-testable without a window.
    fn engine_mode_patch(&self) -> SettingsPatch {
        SettingsPatch {
            engine_mode: Some(self.engine_mode),
            ..SettingsPatch::default()
        }
    }
}

pub struct CascadeApp {
    cmd: Sender<Cmd>,
    reply: Receiver<Reply>,
    conn: Conn,
    status: Option<Status>,
    settings: Option<Settings>,
    library: Vec<LibraryItem>,
    /// Stable 4-hex id — ordinals are display-only : numbers never
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
    /// the polls then skip `library.list`.
    events_live: bool,
    /// One `library.list` fetch is still owed after the stream
    /// (re)connects: events carry *changes*, never the initial list
    /// so the first poll after `EventsLive(true)` must
    /// include the library even though the stream is live (
    /// stateless resync). Cleared when that fetch lands.
    library_resync: bool,
    /// Search box text: client-side filter over title + path.
    search: String,
    /// Whether the search field is currently revealed. The icon-only
    /// button toggles it; a click anywhere else (empty space, a tile)
    /// hides it and clears the query (refinement).
    search_open: bool,
    /// Thumb requests (UI → worker); `None` when no thread runs — the
    /// spawn failed, or the worker exited.
    thumb_tx: Option<Sender<ThumbReq>>,
    /// Thumb results (worker → UI), drained once per `tick`.
    thumb_rx: Receiver<ThumbDone>,
    /// Ids with a decode in flight — one request at a time per id.
    thumb_pending: HashSet<String>,
    /// Ids whose decode failed → the reason shown on hover over the
    /// placeholder. Cleared when the id set changes or the library
    /// is edited; never a banner.
    thumb_failed: HashMap<String, String>,
    /// GPU textures for the grid, LRU-limited.
    textures: TextureLru<egui::TextureHandle>,
    /// The orphan prune already ran after the first successful list
    /// reply — never from an error path.
    pruned: bool,
    /// Which surface the sidebar shows.
    tab: Tab,
    /// UI-local target monitor for the next `Set as wallpaper`.
    /// `None` keeps the daemon's primary→first fallback; this is not a
    /// persisted setting and never starts the engine by itself.
    target_monitor: Option<String>,
    /// Draft settings, synced from the wire only while no write is in
    /// flight.
    form: SettingsForm,
    /// One settings/autostart write is in flight. While true the 1 s poll
    /// cannot overwrite the draft with a pre-write snapshot.
    settings_pending: bool,
    /// A draft exists that has not been sent yet (engine mode always waits
    /// for its button; a second edit can land while another write is
    /// pending). While true the wire snapshot may not replace the form —
    /// otherwise the selection snaps back on the next frame.
    dirty: bool,
    /// Success note for the in-flight write (engine-mode restart wording).
    pending_notice: Option<String>,
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
        // it cannot start, every tile keeps its placeholder and
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
            search_open: false,
            thumb_tx,
            thumb_rx,
            thumb_pending: HashSet::new(),
            thumb_failed: HashMap::new(),
            textures: TextureLru::new(thumbs::LRU_LIMIT),
            pruned: false,
            tab: Tab::Library,
            target_monitor: None,
            form: SettingsForm::from_wire(&Settings::default(), false),
            settings_pending: false,
            dirty: false,
            pending_notice: None,
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

    // worker plumbing ------------------------------------------------

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
        // list, so a removal that raced a decode drops its texture.
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
        // repaint is requested and no poll is sent (architecture ).
        ctx.request_repaint_after(POLL_INTERVAL);
    }

    /// Drain finished thumbnails into GPU textures: `load_texture`
    /// runs only here, on the UI thread — never on the worker.
    fn drain_thumbs(&mut self, ctx: &egui::Context) {
        loop {
            match self.thumb_rx.try_recv() {
                Ok(done) => self.apply_thumb(ctx, done),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The worker exited (it never started, or it died):
                    // every tile keeps its placeholder from now on — no
                    // retry loops.
                    self.thumb_tx = None;
                    break;
                }
            }
        }
    }

    /// One result: forget its pending request, then either build the
    /// texture or remember the failure reason. A result for an
    /// id that left the library is dropped.
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

    /// Adopt a new library list: textures and in-flight requests
    /// for ids that stayed, everything dropped for ids that left, and
    /// all failure reasons cleared when the id set changed — a fresh id
    /// must get a fresh attempt. `prune` is true **only** from a
    /// successful `library.list` reply: the single place an orphan
    /// prune may run (an empty guess from an error path would
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
            // directory. The full prune is on target.
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

    /// Queue one settings write. At most one is in flight, so replies
    /// cannot overwrite a newer draft with an older snapshot.
    fn send_setting(&mut self, patch: SettingsPatch, note: Option<String>) {
        if self.settings_pending {
            return;
        }
        self.settings_pending = true;
        // The draft is now in flight; the answer is the next wire truth.
        self.dirty = false;
        self.pending_notice = note;
        self.send(Cmd::SettingsSet(patch));
    }

    /// Autostart is a separate wire command: the Run key is the oracle
    /// never a field of `SettingsPatch`.
    fn send_autostart(&mut self, enabled: bool) {
        if self.settings_pending {
            return;
        }
        self.settings_pending = true;
        self.dirty = false;
        self.pending_notice = None;
        self.send(Cmd::AutostartSet(enabled));
    }

    /// Whether the next poll must carry `library.list`: always while no
    /// subscription is live, plus the one owed resync fetch after a
    /// (re)connect.
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
                        // A display change can remove the second monitor or
                        // the chosen target. Both are resolved
                        // from the wire snapshot, never guessed locally.
                        self.tab = self.tab.normalize(status.monitors.len());
                        if retain_target(&mut self.target_monitor, &status) {
                            self.notice = Some(
                                "target monitor is no longer attached \u{2014} using the daemon default"
                                    .to_string(),
                            );
                        }
                        self.status = Some(status);
                    }
                    Err(ClientError::NotRunning) => {
                        // The engine is down: forget everything it owned
                        // so no stale list, status values, or thumbnail
                        // textures linger.
                        self.conn = Conn::NotRunning;
                        self.status = None;
                        self.settings = None;
                        // A daemon that died before answering cannot leave
                        // the form frozen: the CLI remains the oracle.
                        self.settings_pending = false;
                        self.dirty = false;
                        self.pending_notice = None;
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
                            // allowed: a *successful* list reply.
                            self.set_library(data.items, true);
                            self.last_snapshot_error = None;
                        }
                        Some(Err(err)) => {
                            self.library_resync = false;
                            self.snapshot_error(err.to_string());
                        }
                        // Skipped only while the event stream is live
                        // *and* no resync is owed — the events own the
                        // list from then on.
                        None => {}
                    }
                    match settings {
                        // While a write is in flight the pre-write snapshot
                        // must not become the new diff base — otherwise the
                        // user's draft would be sent twice or reverted
                        // visually.
                        Ok(settings) if !self.settings_pending => self.settings = Some(settings),
                        Ok(_) => {}
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
            Reply::SettingsChanged(result) => {
                self.settings_pending = false;
                match result {
                    Ok(settings) => {
                        self.settings = Some(settings);
                        if let Some(note) = self.pending_notice.take() {
                            self.notice = Some(note);
                        }
                    }
                    Err(err) => {
                        self.pending_notice = None;
                        self.dirty = false;
                        self.notice = Some(err.to_string());
                    }
                }
                // The write changed state (or failed): refresh status,
                // the conflict warning and the autostart oracle at once.
                self.request_poll_now();
            }
            Reply::AutostartChanged(result) => {
                self.settings_pending = false;
                self.pending_notice = None;
                match result {
                    Ok(autostart) => {
                        if let Some(status) = self.status.as_mut() {
                            status.autostart = autostart;
                        }
                    }
                    Err(err) => {
                        self.dirty = false;
                        self.notice = Some(err.to_string());
                    }
                }
                self.request_poll_now();
            }
            Reply::EventsLive(live) => {
                let became_live = live && !self.events_live;
                self.events_live = live;
                if became_live {
                    // Events are not replayed: resync with
                    // one full poll right after (re)connecting —
                    // full means it *carries* `library.list`, which
                    // `library_resync` forces even while live. Without
                    // this the window could start empty and stay empty
                    // until the next `library.changed` (seen on the
                    // target machine,: a UI started after the
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

    /// One pushed event. The reader thread delivers them
    /// in order; anything the UI cannot use falls back to a poll rather
    /// than trusting a payload it does not understand.
    fn apply_event(&mut self, event: Event) {
        match event.event.as_str() {
            // Mode/pause/applied-video changed: refresh status. The
            // scheduled poll would do it within 1 s anyway — this makes
            // it immediate.
            "state" => self.request_poll_now(),
            "library.changed" => match serde_json::from_value::<LibraryItemsData>(event.data) {
                Ok(data) => {
                    self.set_library(data.items, false);
                    // The list was *edited*: give every failed thumbnail
                    // one fresh attempt — a mutation is the only
                    // thing that can change a file's state behind our
                    // back.
                    self.thumb_failed.clear();
                }
                // Payload disagreement → the same remedy as a version
                // mismatch: refetch, do not guess.
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
            // does not know is ignored, never fatal.
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

    // drawing --------------------------------------------------------

    /// Files dropped onto the window go through the same `library.add`
    /// as the dialog : drag-and-drop).
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
        ui.add_space(10.0);
        ui.label(egui::RichText::new("Cascade").strong().size(20.0));
        ui.add_space(18.0);
        // Three of the four reserved surfaces exist. Monitors only makes
        // sense with >1 monitor; the count comes from `status`,
        // never from a captured machine id.
        // Full-width nav rows, not buttons: the active row gets a tint and
        // a left accent bar, hover only a faint fill (refinement).
        let tab = self.tab;
        if sidebar_nav_item(ui, "Library", tab == Tab::Library) {
            self.tab = Tab::Library;
        }
        ui.add_space(4.0);
        if sidebar_nav_item(ui, "Settings", tab == Tab::Settings) {
            self.tab = Tab::Settings;
        }
        let monitor_count = self
            .status
            .as_ref()
            .map_or(0, |status| status.monitors.len());
        if monitor_count > 1 {
            ui.add_space(4.0);
            if sidebar_nav_item(ui, "Monitors", tab == Tab::Monitors) {
                self.tab = Tab::Monitors;
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            theme::muted(ui, concat!("cascade ", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
        });
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
            Conn::Connected => match self.tab {
                Tab::Library => self.library_screen(ui),
                Tab::Settings => self.settings_screen(ui),
                Tab::Monitors => self.monitor_screen(ui),
            },
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

    /// The Monitors surface. Read-only except for two
    /// actions that reuse existing wire commands: choosing the target for
    /// the next Set (a UI-local value sent in `SetData.monitor`) and
    /// re-targeting the current video with `wallpaper.set --monitor`.
    fn monitor_screen(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Monitors").size(20.0).strong());
        ui.add_space(6.0);
        theme::muted(
            ui,
            "Cascade plays one video on one target monitor. Choose where the next \
             \"Set as wallpaper\" should appear, or use Play here to move the current video.",
        );
        ui.add_space(12.0);

        let Some(status) = self.status.clone() else {
            theme::muted(ui, "Monitor information is unavailable.");
            return;
        };
        let source_id = self.selected.clone().or_else(|| active_source_id(&status));
        let mut choose_target: Option<Option<String>> = None;
        let mut play_here: Option<String> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            if ui
                .radio(
                    self.target_monitor.is_none(),
                    "Daemon default (primary monitor)",
                )
                .clicked()
            {
                choose_target = Some(None);
            }
            theme::muted(
                ui,
                "No monitor id is invented by the UI; the daemon picks primary, then first.",
            );
            ui.add_space(12.0);

            for monitor in &status.monitors {
                let is_target = self.target_monitor.as_deref() == Some(monitor.id.as_str());
                ui.horizontal_wrapped(|ui| {
                    if ui.radio(is_target, monitor.id.as_str()).clicked() {
                        choose_target = Some(Some(monitor.id.clone()));
                    }
                    if monitor.primary {
                        ui.label(egui::RichText::new("PRIMARY").small().color(theme::ACCENT));
                    }
                    theme::muted(ui, &format_monitor_rect(&monitor.rect));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(24.0);
                    if let Some(source) = &monitor.source {
                        theme::muted(
                            ui,
                            &format!(
                                "playing: {} \u{b7} {} \u{b7} {}",
                                source.title,
                                monitor.video.map_or_else(
                                    || "unknown size".to_string(),
                                    |v| format!("{}x{}", v.w, v.h)
                                ),
                                monitor.fps.map_or_else(
                                    || "fps unknown".to_string(),
                                    |fps| format!("{fps:.0} fps")
                                )
                            ),
                        );
                    } else {
                        theme::muted(ui, "idle");
                    }
                    if let Some(hwdec) = monitor.hwdec {
                        theme::muted(
                            ui,
                            if hwdec {
                                "(hardware decode)"
                            } else {
                                "(software decode)"
                            },
                        );
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    let button =
                        ui.add_enabled(source_id.is_some(), egui::Button::new("Play here"));
                    let button = if source_id.is_some() {
                        button.on_hover_text("Send wallpaper.set with this monitor id")
                    } else {
                        button
                    };
                    if button.clicked() {
                        play_here = Some(monitor.id.clone());
                    }
                });
                ui.add_space(10.0);
            }

            if source_id.is_none() {
                theme::muted(ui, "Select a library item first to enable Play here.");
            }
        });

        if let Some(target) = choose_target {
            self.target_monitor = target;
        }
        if let (Some(id), Some(monitor)) = (source_id, play_here) {
            self.send(Cmd::Set(id, Some(monitor)));
        }
    }

    /// The Settings surface. Pure wire
    /// shapes only: every write goes through the worker with the same
    /// commands the CLI uses; nothing is applied locally.
    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        // Adopt the wire snapshot only when no write is in flight AND no
        // draft is waiting. A draft can exist while a write is pending
        // (a second scroll, or the engine-mode selection), so checking
        // `settings_pending` alone lost the user's last edit (fix).
        if !self.settings_pending && !self.dirty {
            if let Some(settings) = self.settings {
                let autostart = self
                    .status
                    .as_ref()
                    .is_some_and(|status| status.autostart.enabled);
                self.form.sync_from_wire(&settings, autostart);
            }
        }
        let Some(settings) = self.settings else {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label("Settings are unavailable");
                ui.add_space(6.0);
                theme::muted(
                    ui,
                    "The engine did not answer `settings get`. Use `cascade settings get` in a terminal, or Retry.",
                );
            });
            return;
        };

        let listening = self
            .status
            .as_ref()
            .is_some_and(|status| status.mode == "listening");
        let autostart_now = self
            .status
            .as_ref()
            .is_some_and(|status| status.autostart.enabled);
        let windows_disabled = self
            .status
            .as_ref()
            .is_some_and(|status| status.autostart.windows_disabled);
        let engine_mode_changing = self.form.engine_mode != settings.engine_mode;
        let conflict = settings.conflict_warning();
        let mut apply_engine = false;
        let mut slider_settled = false;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Settings").strong().size(16.0));
                ui.add_space(8.0);
                ui.separator();

                settings_group(ui, "Playback", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Volume");
                        // The slider owns the draft value directly. A drag
                        // changes it every frame; `dirty` (computed below)
                        // keeps the poll from reverting it, and the write
                        // is sent on release (or on a discrete step).
                        let response = ui.add(
                            egui::Slider::new(&mut self.form.volume, 0..=100)
                                .suffix("%")
                                .trailing_fill(true),
                        );
                        slider_settled =
                            response.drag_stopped() || (response.changed() && !response.dragged());
                        ui.label(format!("{} / 100", self.form.volume));
                    });
                    theme::muted(
                        ui,
                        if listening {
                            "Applies at the next load — no engine to restart."
                        } else {
                            "Applies to the playing video immediately; no restart."
                        },
                    );
                });

                settings_group(ui, "Engine", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Engine mode");
                        egui::ComboBox::from_id_salt("cascade-engine-mode")
                            .selected_text(self.form.engine_mode.as_str())
                            .show_ui(ui, |ui| {
                                for mode in
                                    [PresentationMode::Accurate, PresentationMode::Efficient]
                                {
                                    ui.selectable_value(
                                        &mut self.form.engine_mode,
                                        mode,
                                        mode.as_str(),
                                    );
                                }
                            });
                    });
                    if engine_mode_changing {
                        if listening {
                            theme::muted(ui, "No engine to restart — applied at the next start.");
                        } else {
                            ui.colored_label(
                                theme::WARN,
                                "Changing the engine mode restarts the engine when a video is playing; the desktop is restored during the switch.",
                            );
                        }
                        apply_engine = theme::accent_button(ui, "Apply & restart").clicked();
                    } else {
                        theme::muted(
                            ui,
                            "Changing this restarts the engine when a video is playing; the desktop is restored during the switch.",
                        );
                    }
                });

                settings_group(ui, "Pause rules", |ui| {
                    theme::checkbox(ui, &mut self.form.pause.fullscreen, "Pause on fullscreen");
                    theme::checkbox(
                        ui,
                        &mut self.form.pause.maximized,
                        "Pause on maximized window",
                    );
                    theme::checkbox(
                        ui,
                        &mut self.form.pause.display_off,
                        "Pause when display turns off",
                    );
                    theme::checkbox(ui, &mut self.form.pause.session_lock, "Pause on session lock");
                    theme::checkbox(ui, &mut self.form.pause.screensaver, "Pause on screensaver");
                    theme::checkbox(
                        ui,
                        &mut self.form.pause.remote_session,
                        "Pause on remote session",
                    );
                });

                settings_group(ui, "Power", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Power profile");
                        egui::ComboBox::from_id_salt("cascade-power-profile")
                            .selected_text(self.form.power_profile.as_str())
                            .show_ui(ui, |ui| {
                                for profile in [
                                    PowerProfile::Off,
                                    PowerProfile::Balanced,
                                    PowerProfile::Battery,
                                    PowerProfile::Quality,
                                ] {
                                    ui.selectable_value(
                                        &mut self.form.power_profile,
                                        profile,
                                        profile.as_str(),
                                    );
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("On battery");
                        egui::ComboBox::from_id_salt("cascade-on-battery")
                            .selected_text(self.form.on_battery.as_str())
                            .show_ui(ui, |ui| {
                                for action in [
                                    BatteryAction::Off,
                                    BatteryAction::Pause,
                                    BatteryAction::Stop,
                                    BatteryAction::Continue,
                                ] {
                                    ui.selectable_value(
                                        &mut self.form.on_battery,
                                        action,
                                        action.as_str(),
                                    );
                                }
                            });
                    });
                    if self.form.on_battery == BatteryAction::Stop {
                        ui.colored_label(
                            theme::WARN,
                            "A full stop: the real wallpaper is restored and a zero-compute listener serves the tray until AC returns.",
                        );
                    }
                    if let Some(warning) = &conflict {
                        ui.colored_label(theme::WARN, warning);
                    }
                });

                settings_group(ui, "System", |ui| {
                    theme::checkbox(
                        ui,
                        &mut self.form.restore_wallpaper_on_exit,
                        "Restore wallpaper on exit",
                    );
                    theme::muted(
                        ui,
                        "Desktop is restored on every exit regardless; this is stored state only.",
                    );
                    theme::checkbox(ui, &mut self.form.tray, "Tray icon");
                    theme::checkbox(
                        ui,
                        &mut self.form.autostart,
                        "Start Cascade with Windows",
                    );
                    if windows_disabled {
                        ui.colored_label(theme::WARN, "Disabled in Settings → Apps → Startup");
                    }
                });
            });

        // Apply after the widgets updated the draft: one write in flight,
        // engine mode only through the explicit confirmation above.
        let (patch, autostart_patch) = self.form.changes(&settings, autostart_now, slider_settled);
        let volume_draft = self.form.volume != settings.volume;
        let engine_draft = self.form.engine_mode != settings.engine_mode;
        // Any unsent draft freezes the snapshot. This is the fix for both
        // bugs found on target: the combo selection now survives until the
        // Apply button is clicked, and a scroll made during an in-flight
        // write is still owed after the answer lands.
        self.dirty = volume_draft || patch.is_some() || autostart_patch.is_some() || engine_draft;

        if apply_engine {
            let note = if listening {
                "Engine mode saved — applied at the next start (no engine to restart)."
            } else {
                "Engine mode saved — the daemon restarts to apply it if a video is playing."
            };
            self.send_setting(self.form.engine_mode_patch(), Some(note.to_string()));
        }
        if !self.settings_pending {
            if let Some(patch) = patch {
                self.send_setting(patch, None);
            } else if let Some(enabled) = autostart_patch {
                self.send_autostart(enabled);
            }
        }
    }

    fn library_screen(&mut self, ui: &mut egui::Ui) {
        let mut add = false;
        let was_open = self.search_open;
        // "Library" owns its row alone — no count, no button beside it
        // (refinement).
        ui.add_space(2.0);
        ui.label(egui::RichText::new("Library").strong().size(20.0));
        ui.add_space(8.0);

        // Action row below the title: compact Add + icon-only search
        // toggle on the left; the revealed field sits directly after the
        // toggle at exactly the same box height as the two buttons.
        let mut search_rects: Vec<egui::Rect> = Vec::new();
        ui.horizontal(|ui| {
            let button = egui::Button::new(egui::RichText::new("+ Add").size(13.0))
                .fill(theme::ACCENT)
                .min_size(vec2(ADD_W, ACTION_H));
            if ui.add(button).clicked() {
                add = true;
            }

            let toggle = search_button(ui, ACTION_H);
            search_rects.push(toggle.rect);
            if toggle.clicked() {
                self.search_open = !self.search_open;
                if !self.search_open {
                    self.search.clear();
                }
            }

            if self.search_open {
                let width = SEARCH_W.min(ui.available_width());
                // No right-pinning: the field sits immediately after the
                // magnifier, sharing the same action row and box height
                // (Settings/search pass).
                let field = egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search title or path\u{2026}")
                    .desired_width(width)
                    .min_size(vec2(width, ACTION_H));
                search_rects.push(ui.add(field).rect);
            }
        });
        ui.add_space(10.0);

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
        } else {
            self.library_grid(ui);
        }

        // A click anywhere outside the search affordances hides the
        // field — including a click on a thumbnail (the "active" case
        // the user called out). Clicks on the toggle itself are excluded
        // so the opening frame is not immediately undone (refinement).
        if was_open && self.search_open {
            let click = ui.ctx().input(|input| {
                if input.pointer.any_click() {
                    input.pointer.latest_pos()
                } else {
                    None
                }
            });
            if let Some(pos) = click {
                if !search_rects.iter().any(|rect| rect.contains(pos)) {
                    self.search_open = false;
                    self.search.clear();
                }
            }
        }

        if add {
            self.add_files();
        }
    }

    /// The virtualized tile grid: `show_rows`
    /// renders only the visible rows, each visible tile requests its
    /// thumbnail on render, and every action is collected during
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
        // The horizontal gap is the current item spacing
        // (8 pt default); `show_rows` adds the same spacing vertically,
        // so the pitch matches the row advance exactly.
        let spacing = ui.spacing().item_spacing;
        let columns =
            (((ui.available_width() + spacing.x) / (TILE_W + spacing.x)).floor() as usize).max(1);
        let total_rows = filtered.len().div_ceil(columns);
        // No action row below the grid any more: the whole panel height
        // belongs to the tiles (refinement).
        let grid_h = ui.available_height().max(MIN_GRID_H);

        let mut enqueue: Vec<ThumbReq> = Vec::new();
        let mut select: Option<String> = None;
        let mut set_row: Option<String> = None;
        let mut remove_row: Option<String> = None;
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
                            // Request-on-render: only visible tiles
                            // ask, each id asks once, and a failure is
                            // never re-asked until the list changes
                            // no retry loops.
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
                            let tile = LibraryTile {
                                id: &item.id,
                                title: &item.title,
                                meta: &meta,
                                path: &item.path,
                                selected: self.selected.as_deref() == Some(item.id.as_str()),
                                texture,
                                failure,
                            }
                            .show(ui);
                            // Show the menu *before* gating the card:
                            // `Popup::menu` applies its open/toggle command
                            // inside `show`, so skipping the call on the
                            // click frame would never open the popup.
                            egui::Popup::menu(&tile.info).show(|ui| {
                                if ui.button("Set as wallpaper").clicked() {
                                    set_row = Some(item.id.clone());
                                }
                                if ui.button("Remove from library").clicked() {
                                    remove_row = Some(item.id.clone());
                                }
                            });
                            // The info button wins over the tile: clicking
                            // it must not also select or set the wallpaper.
                            if tile.info.clicked() {
                                continue;
                            }
                            if tile.tile.clicked() {
                                select = Some(item.id.clone());
                            }
                            if tile.tile.double_clicked() {
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
            self.send(Cmd::Set(id, self.target_monitor.clone()));
        }
        if let Some(id) = remove_row {
            self.send(Cmd::Remove(id));
        }
        // Hand the collected requests to the thumb thread. The
        // worker wakes the UI itself when a result arrives.
        if let Some(tx) = self.thumb_tx.as_ref() {
            for req in enqueue {
                let _ = tx.send(req);
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

        // The bottom status strip is gone (refinement): mode, engine
        // mode, fps, memory and volume are CLI facts, not UI chrome.

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(16.0))
            .show(ui, |ui| self.central(ui));
    }

    /// Paint the app background with the palette instead of egui's
    /// translucent default.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}

// drawing helpers -----------------------------------------------------

/// Hand-painted icon-only search button: a zoom glass drawn as shapes,
/// not a text glyph, so it renders at any DPI without font coverage
/// (refinement).
fn search_button(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        6.0,
        if response.hovered() {
            theme::BORDER
        } else {
            theme::SURFACE
        },
    );
    let center = rect.center() - vec2(1.5, 1.5);
    let radius = size * 0.22;
    let stroke = egui::Stroke::new(1.5, theme::TEXT);
    painter.circle_stroke(center, radius, stroke);
    let start = center + vec2(radius * 0.72, radius * 0.72);
    painter.line_segment([start, start + vec2(size * 0.16, size * 0.16)], stroke);
    response.on_hover_text("Search")
}

/// One Settings category: a slightly larger bold heading, then its
/// controls indented forward so they do not align with the heading. The
/// manual indent (instead of `ui.indent`) keeps the layout free of egui's
/// optional guide line; only presentation changes, no wire shape (
/// Settings pass).
fn settings_group(ui: &mut egui::Ui, title: &str, contents: impl FnOnce(&mut egui::Ui)) {
    ui.label(egui::RichText::new(title).size(15.0).strong());
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(contents);
    });
    ui.add_space(10.0);
}

/// Geometry straight from the wire rect — no assumed origin or size
/// Negative coordinates are normal for a monitor left of the
/// primary one and must be shown, not clamped.
fn format_monitor_rect(rect: &Rect) -> String {
    format!("{},{} {}×{}", rect.x, rect.y, rect.w, rect.h)
}

/// The library id currently playing, if any. The daemon attaches `source`
/// only to the active monitor row, so this is never a guess.
fn active_source_id(status: &Status) -> Option<String> {
    status
        .monitors
        .iter()
        .find_map(|monitor| monitor.source.as_ref().map(|source| source.id.clone()))
}

/// Drop a UI-local target that no longer exists after a display change.
/// Returns true when it was cleared so the caller can report it honestly.
fn retain_target(target: &mut Option<String>, status: &Status) -> bool {
    let Some(id) = target.as_deref() else {
        return false;
    };
    if status.monitors.iter().any(|monitor| monitor.id == id) {
        return false;
    }
    *target = None;
    true
}

// pure helpers (unit-tested) ------------------------------------------

/// One full-width sidebar row. A hand-painted nav item rather than a
/// `selectable_label`/`Button`: no border, no button padding, a tinted bar
/// with a left accent for the active tab, and a faint fill on hover
/// (refinement).
fn sidebar_nav_item(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), SIDEBAR_ITEM_H),
        egui::Sense::click(),
    );
    let fill = if active {
        theme::ACCENT_SOFT
    } else if response.hovered() {
        theme::SURFACE
    } else {
        egui::Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, fill);
    if active {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
            0.0,
            theme::ACCENT,
        );
    }
    painter.text(
        rect.left_center() + vec2(14.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(16.0),
        if active { theme::TEXT } else { theme::MUTED },
    );
    response.clicked()
}

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

/// Client-side search: case-insensitive substring over title and
/// path. An empty or whitespace-only query matches everything — pure,
/// unit-tested.
fn matches_query(item: &LibraryItem, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    item.title.to_lowercase().contains(&needle) || item.path.to_lowercase().contains(&needle)
}

/// One grid tile: a large thumbnail, a wrapping title and a wrapping
/// metadata line inside one `allocate_exact_size` (refinement).
/// Painted by hand so the row's cursor math stays exactly one allocation.
struct LibraryTile<'a> {
    /// Stable id, used to keep the menu popup anchored to the same widget
    /// even when search renumbers or scroll virtualization moves the tile.
    id: &'a str,
    title: &'a str,
    meta: &'a str,
    path: &'a str,
    selected: bool,
    /// `Some` = decoded texture; `None` = placeholder (decoding/failed).
    texture: Option<&'a egui::TextureHandle>,
    /// Failure reason for the hover; `None` while still decoding.
    failure: Option<&'a str>,
}

/// The card and its (i) button are separate click targets. The button
/// owns the item menu; the card keeps click-to-select / double-click-set.
struct TileInteractions {
    tile: egui::Response,
    info: egui::Response,
}

impl LibraryTile<'_> {
    fn show(self, ui: &mut egui::Ui) -> TileInteractions {
        let (rect, tile) = ui.allocate_exact_size(vec2(TILE_W, ROW_H), egui::Sense::click());
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
                        egui::FontId::proportional(14.0),
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
            4.0,
            egui::Color32::TRANSPARENT,
            stroke,
            egui::StrokeKind::Inside,
        );

        // The two text lines live in a scoped child ui whose cursor
        // starts under the thumbnail; `ui.add` gives each label only the
        // height it actually needs, so a one-line title sits directly on
        // top of the metadata (a 4 pt gap) instead of being pushed down by
        // a fixed two-line reservation. Long titles and codec/fps strings
        // still wrap instead of truncating (refinement).
        //
        // CORRECTION 1 (2026-11-22, user-observed): this scope originally
        // inherited the parent's horizontal layout, so the two labels sat
        // side by side and the metadata was squeezed into 2-3 characters
        // per line. `top_down` is what actually stacks them.
        //
        // CORRECTION 2 (same day, user-observed): `scope_builder` calls
        // `advance_cursor_after_rect(child_ui.min_rect())`. Since the
        // tile was already allocated above, that advanced the parent
        // cursor a second time and the next column overlapped this one.
        // `new_child` allocates nothing in the parent — exactly what a
        // paint-only overlay inside an already-allocated tile needs.
        let text_top = rect.min + vec2(0.0, THUMB_H + 6.0);
        let text_rect = egui::Rect::from_min_max(
            text_top,
            egui::pos2(rect.max.x, (text_top.y + TEXT_H).min(rect.max.y)),
        );
        let mut text_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        text_ui.add(
            egui::Label::new(egui::RichText::new(self.title).size(15.0).strong())
                .wrap()
                .halign(egui::Align::Min),
        );
        text_ui.add_space(TEXT_GAP);
        text_ui.add(
            egui::Label::new(
                egui::RichText::new(self.meta)
                    .size(12.0)
                    .color(theme::MUTED),
            )
            .wrap()
            .halign(egui::Align::Min),
        );

        // The menu button sits inside the thumbnail, top-right: a plain
        // circle filled with the sidebar-active tint, no outline, three
        // vertical dots. A per-item explicit id keeps the popup anchored
        // across search renumbering and scroll virtualization.
        let info_rect = egui::Rect::from_min_size(
            egui::pos2(thumb.max.x - INFO_SIZE - 8.0, thumb.min.y + 8.0),
            vec2(INFO_SIZE, INFO_SIZE),
        );
        let info_id = egui::Id::unique(("library-menu", self.id));
        let info = ui.interact(info_rect, info_id, egui::Sense::click());
        if ui.is_rect_visible(info_rect) {
            let painter = ui.painter();
            let center = info_rect.center();
            let radius = INFO_SIZE / 2.0;
            let fill = if info.hovered() {
                theme::ACCENT
            } else {
                theme::ACCENT_SOFT
            };
            painter.circle_filled(center, radius, fill);
            for offset in [-5.0, 0.0, 5.0] {
                painter.circle_filled(center + vec2(0.0, offset), 1.4, theme::TEXT);
            }
        }
        let info = info.on_hover_text("Item actions");

        // Hover: the path always; a failed decode adds its reason
        // the placeholder explains itself, never a banner.
        let tile = match self.failure {
            Some(reason) => tile.on_hover_text(format!("{}\n{}", self.path, reason)),
            None => tile.on_hover_text(self.path),
        };
        TileInteractions { tile, info }
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

// worker thread -------------------------------------------------------

/// The single IPC thread: strictly serial, so replies arrive in
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
                    // status + settings only.
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
            Cmd::Set(id, monitor) => Reply::SetDone(ask_data(
                "wallpaper.set",
                SetData {
                    path: None,
                    id: Some(id),
                    monitor,
                },
            )),
            Cmd::StartEngine => Reply::Started(start_engine()),
            Cmd::SettingsSet(patch) => Reply::SettingsChanged(ask_data("settings.set", patch)),
            Cmd::AutostartSet(enabled) => {
                Reply::AutostartChanged(ask_data("autostart.set", EnabledData { enabled }))
            }
        };
        if tx.send(reply).is_err() {
            break; // the window is gone
        }
    }
}

/// The event subscription thread: holds one persistent
/// connection to `cascade-evt-{session}`, forwards every frame as
/// [`Reply::Event`], and reports connectivity as [`Reply::EventsLive`].
/// It never issues a request — the worker keeps its serial command order
/// Reconnects follow: 500 ms doubling to 5 s, never
/// a spin; a fresh connection means a stateless resync on the UI side.
///
/// `ctx` is woken after every delivery: egui draws only when something
/// asks for a frame, and the idle schedule is one frame per second —
/// without the wake an event waits up to `POLL_INTERVAL` before it is
/// even read (measured 2026-11-21: a library row appeared 0–1000 ms
/// late).
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
        // and try again — never a spin.
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
/// protocol — the same remedy as a version mismatch.
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

/// Start the engine through the same command the CLI uses:
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
    fn item_meta_is_complete_without_an_ordinal() {
        let library = [item("aa11"), item("bb22")];
        // the displayed title is the bare file title — no ordinal.
        assert_eq!(library[0].title, "aa11");
        let meta = item_meta(&library[0]);
        assert!(meta.contains("1920x1080"), "{meta}");
        assert!(meta.contains("H264"), "{meta}");
        assert!(meta.contains("1:05"), "{meta}");
    }

    #[test]
    fn palette_matches_the_design_table() {
        // a silent hex drift would change the theme.
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
        assert_eq!(
            theme::ACCENT_SOFT,
            egui::Color32::from_rgb(0x22, 0x24, 0x4A)
        );
    }

    // event consumption ------------------------------

    use cascade_core::ipc::{AutostartStatus, HostStatus, LibraryRef, MemoryStatus, MonitorStatus};
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

    fn monitor(id: &str, primary: bool, rect: Rect, source: Option<LibraryRef>) -> MonitorStatus {
        MonitorStatus {
            id: id.to_string(),
            rect,
            primary,
            source,
            hwdec: None,
            fps: None,
            dropped: None,
            video: None,
            engine: None,
        }
    }

    fn snapshot(library: Option<Result<LibraryItemsData, ClientError>>) -> Reply {
        Reply::Snapshot {
            status: Ok(status_fixture()),
            library,
            settings: Ok(Settings::default()),
        }
    }

    fn snapshot_with_status(status: Status) -> Reply {
        Reply::Snapshot {
            status: Ok(status),
            library: None,
            settings: Ok(Settings::default()),
        }
    }

    #[test]
    fn monitors_tab_only_survives_a_real_multimonitor_snapshot() {
        assert_eq!(Tab::Library.normalize(1), Tab::Library);
        assert_eq!(Tab::Settings.normalize(0), Tab::Settings);
        assert_eq!(Tab::Monitors.normalize(1), Tab::Library);
        assert_eq!(Tab::Monitors.normalize(2), Tab::Monitors);
    }

    #[test]
    fn monitor_geometry_comes_from_the_wire_rect() {
        assert_eq!(
            format_monitor_rect(&Rect::new(-1920, 120, 1920, 1080)),
            "-1920,120 1920×1080"
        );
        assert_eq!(
            format_monitor_rect(&Rect::new(0, 0, 1366, 768)),
            "0,0 1366×768"
        );
    }

    #[test]
    fn active_source_and_target_follow_the_status_snapshot() {
        let mut status = status_fixture();
        status.monitors = vec![
            monitor("\\\\.\\DISPLAY1", true, Rect::new(0, 0, 1366, 768), None),
            monitor(
                "\\\\.\\DISPLAY2",
                false,
                Rect::new(-1920, 0, 1920, 1080),
                Some(LibraryRef {
                    id: "aa11".to_string(),
                    title: "clip".to_string(),
                }),
            ),
        ];
        assert_eq!(active_source_id(&status).as_deref(), Some("aa11"));
        let mut target = Some("\\\\.\\DISPLAY2".to_string());
        assert!(!retain_target(&mut target, &status));
        assert_eq!(target.as_deref(), Some("\\\\.\\DISPLAY2"));

        status.monitors.pop();
        assert!(retain_target(&mut target, &status));
        assert_eq!(target, None);
        assert_eq!(active_source_id(&status), None);
    }

    #[test]
    fn snapshot_normalizes_tab_and_stale_target() {
        let mut app = CascadeApp::detached();
        let mut status = status_fixture();
        status.monitors = vec![monitor(
            "\\\\.\\DISPLAY1",
            true,
            Rect::new(0, 0, 1366, 768),
            None,
        )];
        app.tab = Tab::Monitors;
        app.target_monitor = Some("\\\\.\\DISPLAY9".to_string());
        app.apply(snapshot_with_status(status));
        assert_eq!(app.tab, Tab::Library, "one monitor closes the surface");
        assert_eq!(app.target_monitor, None, "a vanished id is never kept");
        assert!(app
            .notice
            .as_deref()
            .is_some_and(|text| text.contains("no longer attached")));
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
            "a state event refreshes the status immediately"
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
            "events are not replayed: one stateless resync after reconnect"
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
        // carry changes, never the initial list.
        assert!(
            app.poll_carries_library(),
            "the first poll always fetches the library"
        );

        // If the event thread wins the race before that poll is sent,
        // the resync still has to carry library.list : the UI
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

    // search + thumbnail state ---------------------

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
        // dropped — a fresh id gets exactly one fresh attempt.
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

    // Settings surface -------------------------------

    #[test]
    fn form_diff_sends_only_edited_fields_and_separates_autostart() {
        let base = Settings::default();
        let mut form = SettingsForm::from_wire(&base, false);
        assert_eq!(
            form.changes(&base, false, true),
            (None, None),
            "a fresh form is idle"
        );

        form.pause.fullscreen = false;
        form.tray = false;
        form.autostart = true;
        let (patch, autostart) = form.changes(&base, false, true);
        let patch = patch.expect("two edited fields");
        assert_eq!(
            patch.pause,
            Some(PausePatch {
                fullscreen: Some(false),
                maximized: Some(base.pause.maximized),
                display_off: Some(base.pause.display_off),
                session_lock: Some(base.pause.session_lock),
                screensaver: Some(base.pause.screensaver),
                remote_session: Some(base.pause.remote_session),
            })
        );
        assert_eq!(patch.tray, Some(false));
        assert_eq!(patch.volume, None);
        assert_eq!(patch.engine_mode, None, "engine mode needs its own button");
        assert_eq!(autostart, Some(true));
    }

    #[test]
    fn slider_draft_and_engine_mode_draft_are_not_sent_implicitly() {
        let base = Settings::default();
        let mut form = SettingsForm::from_wire(&base, false);
        form.volume = 42; // live slider position, gesture not settled yet
        form.engine_mode = PresentationMode::Efficient; // awaiting confirmation
        assert_eq!(
            form.changes(&base, false, false),
            (None, None),
            "no field is written just because its widget moved"
        );

        let (patch, _) = form.changes(&base, false, true);
        assert_eq!(
            patch.and_then(|patch| patch.volume),
            Some(42),
            "drag release sends the live slider value"
        );

        let explicit = form.engine_mode_patch();
        assert_eq!(explicit.engine_mode, Some(PresentationMode::Efficient));
        assert_eq!(
            explicit.volume, None,
            "the explicit patch carries one field"
        );
    }

    #[test]
    fn dirty_draft_blocks_the_snapshot_thatused_to_snap_it_back() {
        // Regression, 2026-11-22, target: a scroll made while an earlier
        // write was in flight was discarded when the first answer landed.
        let mut app = CascadeApp::detached();
        let stale = Settings::default(); // volume 0, the pre-write truth
        app.settings = Some(stale);
        app.form.volume = 70; // second scroll, not sent yet
        app.settings_pending = true;
        app.dirty = true;

        // The screen-side guard is pure: a dirty draft must not adopt this.
        let may_sync = !app.settings_pending && !app.dirty;
        assert!(!may_sync, "the 1 s poll cannot overwrite an unsent draft");

        // Once the answer lands and no new draft exists, sync resumes.
        app.settings_pending = false;
        app.dirty = false;
        let newer = Settings {
            volume: 35,
            ..Settings::default()
        };
        app.form.sync_from_wire(&newer, false);
        assert_eq!(app.form.volume, 35, "the acknowledged wire value wins");
    }

    #[test]
    fn engine_mode_selection_survives_until_the_explicit_apply() {
        // Regression, 2026-11-22, target: selecting "accurate" reverted to
        // "efficient" because engine_mode is excluded from the auto-diff.
        let wire = Settings {
            engine_mode: PresentationMode::Efficient,
            ..Settings::default()
        };
        let mut app = CascadeApp::detached();
        app.settings = Some(wire);
        app.form.engine_mode = PresentationMode::Accurate;
        // The screen marks the unsent engine draft dirty right away.
        app.dirty = app.form.engine_mode != wire.engine_mode;
        assert!(app.dirty);
        assert!(!app.settings_pending, "no write was sent by the combo");

        let may_sync = !app.settings_pending && !app.dirty;
        assert!(!may_sync, "the selection survives the next frame");
        assert_eq!(app.form.engine_mode, PresentationMode::Accurate);

        assert_eq!(
            app.form.engine_mode_patch().engine_mode,
            Some(PresentationMode::Accurate),
            "Apply & restart sends exactly that selection"
        );
    }

    #[test]
    fn pending_write_ignores_a_stale_snapshot_and_reports_failure() {
        let mut app = CascadeApp::detached();
        let mut settings = Settings::default();
        app.settings = Some(settings);
        app.settings_pending = true;
        settings.volume = 80;
        app.apply(Reply::Snapshot {
            status: Ok(status_fixture()),
            library: None,
            settings: Ok(settings),
        });
        assert_eq!(
            app.settings.map(|settings| settings.volume),
            Some(0),
            "the pre-write snapshot stays the diff base until the answer"
        );

        app.apply(Reply::SettingsChanged(Err(ClientError::Timeout)));
        assert!(!app.settings_pending);
        assert!(app.notice.is_some(), "the failed write is visible");
    }

    #[test]
    fn settings_answer_clears_pending_and_shows_the_restart_note() {
        let mut app = CascadeApp::detached();
        app.settings_pending = true;
        app.pending_notice = Some("restart note".to_string());
        let settings = Settings {
            engine_mode: PresentationMode::Efficient,
            ..Settings::default()
        };
        app.apply(Reply::SettingsChanged(Ok(settings)));
        assert!(!app.settings_pending);
        assert_eq!(app.notice.as_deref(), Some("restart note"));
        assert_eq!(
            app.settings.map(|settings| settings.engine_mode),
            Some(PresentationMode::Efficient)
        );
    }

    #[test]
    fn autostart_answer_updates_the_run_key_oracle_only() {
        let mut app = CascadeApp::detached();
        app.status = Some(status_fixture());
        app.settings_pending = true;
        app.apply(Reply::AutostartChanged(Ok(AutostartStatus {
            enabled: false,
            windows_disabled: true,
        })));
        assert!(!app.settings_pending);
        let status = app.status.expect("status still present");
        assert!(!status.autostart.enabled);
        assert!(status.autostart.windows_disabled);
        assert!(
            app.settings.is_none(),
            "autostart never invents a settings payload"
        );
    }

    #[test]
    fn engine_down_releases_a_stuck_write() {
        let mut app = CascadeApp::detached();
        app.settings_pending = true;
        app.pending_notice = Some("waiting".to_string());
        app.apply(Reply::Snapshot {
            status: Err(ClientError::NotRunning),
            library: None,
            settings: Err(ClientError::NotRunning),
        });
        assert!(!app.settings_pending);
        assert_eq!(app.pending_notice, None);
    }
}
