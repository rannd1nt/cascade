//! The daemon: own the control window, attach the host, run the message loop.
//!
//! The loop waits with `MsgWaitForMultipleObjectsEx` and wakes every two
//! seconds instead of blocking in `GetMessageW`. That tick is the watchdog:
//! an Explorer restart recreates `Progman` and every `WorkerW`, a resolution
//! change moves the monitors, and another wallpaper app can push its window
//! above ours. All of those show up as "the host is no longer healthy" and are
//! repaired in place, so the user never has to restart Cascade after a crash,
//! a logon, or an Explorer restart.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use cascade_core::ipc::{no_data, Response, Settings};
use cascade_core::rule_engine::{RuleEngine, SessionState};
use cascade_core::{BackendResult, HostSurface, MediaBackend, MonitorId, PlaybackOptions};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{COLORREF, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};
use windows_sys::Win32::System::Threading::{WaitOnAddress, WakeByAddressSingle};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostMessageW, TranslateMessage,
    MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_CLOSE, WM_QUIT,
};

use crate::config;
use crate::control::{self, ControlWindow};
use crate::host::{self, Host, HostOptions};
use crate::ipc::{Command, PipeChannels, PipeServer, ServerMsg, StatusSnapshot, QUIT_FLUSH_WAIT};
use crate::monitor::{self, MonitorInfo};
use crate::registry;
use crate::repaint;
use crate::shell::{self, ShellError, ShellSurface};
use crate::state::{DaemonState, Store};
use crate::wallpaper::WallpaperSnapshot;
use crate::{derr, dlog};

/// How often the watchdog checks the desktop.
const WATCHDOG_TICK: Duration = Duration::from_millis(2_000);

/// How long a single repair may wait for Explorer to come back.
const RECOVERY_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub color: COLORREF,
    pub repaint_level: u32,
    pub shell_timeout: Duration,
    pub host_mode: host::HostModePreference,
    /// `--video <path>` (design-m2 §2.2). Not `Copy` any more: a `PathBuf`
    /// rides along. `None` means "replay the persisted path, if any".
    pub video: Option<PathBuf>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            color: HostOptions::default().color,
            repaint_level: 1,
            shell_timeout: Duration::from_secs(60),
            host_mode: host::HostModePreference::Auto,
            video: None,
        }
    }
}

/// Runtime-only playback flags — deliberately separate from the persisted
/// `Settings` (design-m4 D10). `user_paused` lives for this session only,
/// exactly like the M3 lock behaviour: it never touches `config.toml`.
#[derive(Clone, Copy, Debug, Default)]
struct Flags {
    user_paused: bool,
}

/// Builds a media backend on demand — the first `wallpaper.set` on a
/// daemon that started without a video needs one. `cascade-win` may not
/// depend on `cascade-media` (AGENTS §4), so the binary passes this
/// factory in. The path lets the binary choose per source later; `None`
/// means no backend is available and the desktop keeps its colour.
pub type BackendFactory = Box<dyn Fn(&Path) -> Option<Box<dyn MediaBackend>> + Send>;

#[derive(Debug)]
pub enum DaemonError {
    AlreadyRunning(usize),
    Control(u32),
    Pipe(u32),
    Shell(ShellError),
    Host(host::HostError),
    NoMonitors,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(hwnd) => write!(
                f,
                "Cascade is already running (control window 0x{hwnd:x}); use `cascade restore` or `cascade stop`"
            ),
            Self::Control(code) => write!(f, "could not create the daemon window (error {code})"),
            Self::Pipe(code) => write!(
                f,
                r"could not open the control pipe (error {code}); if Cascade was just closed, wait a second and retry; if it persists, run `cascade doctor`"
            ),
            Self::Shell(err) => write!(f, "{err}"),
            Self::Host(err) => write!(f, "{err}"),
            Self::NoMonitors => write!(f, "no monitors were reported by the system"),
        }
    }
}

impl std::error::Error for DaemonError {}

static CONTROL_HWND: AtomicIsize = AtomicIsize::new(0);

/// Set to 1 by [`ExitSignal::drop`] once the exit path has finished the
/// repaint and the state save. The console-control handler waits on this
/// address before returning, because Windows force-terminates the process
/// as soon as the handler returns for `CTRL_CLOSE` / logoff / shutdown
/// (measured on build 26200.9711: the process died between `stopping` and
/// the state save, leaving `running=true` behind).
static EXIT_DONE: AtomicU32 = AtomicU32::new(0);

/// How long the console-control handler may wait for the exit path. The
/// normal path finishes in well under a second; the cap keeps a hung
/// repaint from blocking console shutdown forever.
const EXIT_WAIT: Duration = Duration::from_secs(5);

unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    // Posting keeps the first half short: the main thread does the work.
    let hwnd = CONTROL_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() {
        // SAFETY: the control window belongs to this process and outlives the
        // handler; PostMessageW only enqueues.
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    }

    // For these signals Windows kills the process the moment this handler
    // returns, so wait until the exit path has repainted the desktop and
    // saved the state. CTRL_C / CTRL_BREAK do not kill on return (the
    // handler returns TRUE = handled) and must not block: the main thread
    // needs the console to print the exit report.
    if matches!(
        ctrl_type,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    ) {
        let deadline = Instant::now() + EXIT_WAIT;
        while EXIT_DONE.load(Ordering::Acquire) == 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = (deadline - now).as_millis().min(u32::MAX as u128) as u32;
            // SAFETY: both pointers reference live, correctly sized objects
            // (`EXIT_DONE` and a stack local) that outlive this call.
            unsafe {
                WaitOnAddress(
                    &EXIT_DONE as *const AtomicU32 as *const c_void,
                    &0u32 as *const u32 as *const c_void,
                    std::mem::size_of::<AtomicU32>(),
                    remaining,
                );
            }
        }
    }
    1
}

/// Signals [`EXIT_DONE`] when dropped so a waiting console-control handler
/// can return. Created at the top of [`run`]; every exit path — `Ok`, `Err`
/// from an early `?`, and panic unwinding — drops it, which is why the
/// repaint and state save are never cut short.
struct ExitSignal;

impl Drop for ExitSignal {
    fn drop(&mut self) {
        EXIT_DONE.store(1, Ordering::Release);
        // SAFETY: address of a static that lives for the whole process.
        unsafe { WakeByAddressSingle(&EXIT_DONE as *const AtomicU32 as *const c_void) };
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        derr!("cascade: panic: {info}");
        // Best effort: never panic inside the panic hook.
        host::emergency_cleanup(2);
    }));
}

fn print_attach(host: &Host) {
    dlog!(
        "attached: mode={} progman=0x{:x} workerw=0x{:x} defview=0x{:x}, {} host window(s)",
        host.surface().mode.label(),
        host.surface().progman as usize,
        host.surface().workerw as usize,
        host.surface().defview as usize,
        host.windows().len()
    );
    for line in host.describe() {
        dlog!("  {line}");
    }
}

fn print_repaint(label: &str, report: &repaint::RepaintReport) {
    for attempt in &report.attempts {
        dlog!(
            "  {label} L{} {}: {} ({})",
            attempt.level,
            attempt.action,
            if attempt.ok { "ok" } else { "failed" },
            attempt.detail
        );
    }
}

fn attach(
    surface: ShellSurface,
    monitors: &[MonitorInfo],
    opts: &DaemonOptions,
) -> Result<Host, host::HostError> {
    Host::attach(
        surface,
        monitors,
        &HostOptions {
            color: opts.color,
            mode: opts.host_mode,
        },
    )
}

/// Wait up to [`RECOVERY_WINDOW`] for Explorer, then attach again.
///
/// Used by both the explicit `restore` request and the watchdog. Returning
/// `None` is not fatal: the daemon stays alive and retries on the next tick.
fn reattach(monitors: &[MonitorInfo], opts: &DaemonOptions) -> Option<(ShellSurface, Host)> {
    let started = Instant::now();
    while started.elapsed() < RECOVERY_WINDOW {
        match shell::discover(Duration::from_millis(750)) {
            Ok(surface) => match attach(surface, monitors, opts) {
                Ok(host) => return Some((surface, host)),
                Err(err) => derr!("reattach: {err}"),
            },
            Err(err) => derr!("reattach: {err}"),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// Build the render-loop half of a `status` snapshot from current state.
/// Mode and reasons come from the one pause oracle (design-m4 §2.6);
/// memory and autostart are read on demand by the pipe thread instead
/// (`design-m4` §2.2), so this stays cheap enough for the 2 s watchdog.
fn current_snapshot(
    host: &Host,
    monitors: &[MonitorInfo],
    opts: &DaemonOptions,
    settings: &Settings,
    flags: &Flags,
) -> StatusSnapshot {
    let desired = RuleEngine::evaluate(session_state(settings, flags, control::session_locked()));
    // `!playing` outranks host health: a paused engine on an unhealthy
    // host is still "paused" — the watchdog reports health separately.
    let mode = if !desired.playing {
        "paused"
    } else if host.is_healthy() {
        "attached"
    } else {
        "degraded"
    };
    let paused_reasons = if desired.playing {
        Vec::new()
    } else {
        vec![desired.pause_reason.map_or_else(
            || "paused".to_string(),
            |reason| reason.as_str().to_string(),
        )]
    };
    StatusSnapshot {
        mode: mode.to_string(),
        paused_reasons,
        monitors: monitors.to_vec(),
        host_strategy: host.surface().mode.label().to_string(),
        repaint_level: opts.repaint_level,
        settings: *settings,
    }
}

/// Publish the snapshot to the pipe thread only when it changed, so the
/// channel carries no traffic while the desktop is idle.
fn refresh_status(
    tx: &mpsc::Sender<ServerMsg>,
    published: &mut Option<Arc<StatusSnapshot>>,
    host: &Host,
    monitors: &[MonitorInfo],
    opts: &DaemonOptions,
    settings: &Settings,
    flags: &Flags,
) {
    let next = Arc::new(current_snapshot(host, monitors, opts, settings, flags));
    if published.as_ref().is_some_and(|prev| **prev == *next) {
        return;
    }
    if tx.send(ServerMsg::Snapshot(Arc::clone(&next))).is_err() {
        // The pipe thread is gone; keep the desktop running and stop trying.
        derr!("ipc: message channel closed; status will stop updating");
        return;
    }
    *published = Some(next);
}

/// One watchdog tick. Returns `true` when the desktop is healthy afterwards.
fn watchdog_tick(
    host: &mut Host,
    surface: &mut ShellSurface,
    monitors: &[MonitorInfo],
    opts: &DaemonOptions,
    snapshot: &WallpaperSnapshot,
    backend: &mut BackendGuard,
    assigned: Option<&str>,
) -> bool {
    if host.is_healthy() {
        return true;
    }
    derr!("watchdog: host window is no longer healthy; repairing");

    // Cheap repair first: another window may simply have been raised above us.
    if shell::surface_is_alive(surface) {
        host.restack();
        if host.is_healthy() {
            dlog!("watchdog: z-order restored");
            return true;
        }
    }

    // Detach backend before destroying host windows so engine Shutdown()
    // runs while its playback HWND is still alive (design-m2 §3.3).
    if let Err(err) = backend.detach_surface() {
        derr!("watchdog: backend detach failed: {err}");
    }

    host.detach();
    if shell::surface_is_alive(surface) {
        let report = repaint::ladder(surface, snapshot, 1);
        print_repaint("watchdog", &report);
    }

    match reattach(monitors, opts) {
        Some((new_surface, new_host)) => {
            *surface = new_surface;
            *host = new_host;
            print_attach(host);
            attach_backend(backend, host, assigned);
            true
        }
        None => {
            derr!("watchdog: Explorer is not back yet; retrying on the next tick");
            false
        }
    }
}

/// Owns the optional media backend so Media Foundation is shut down on
/// every exit path (design-m2 §3.2: `MFStartup` runs in the constructor).
struct BackendGuard {
    backend: Option<Box<dyn MediaBackend>>,
    /// True only after `load` succeeded. Without a validated source the
    /// backend stays out of the host path entirely, so a run with no video
    /// behaves exactly like M1 (design-m2 §2.2).
    loaded: bool,
}

impl BackendGuard {
    fn take(&mut self) -> Option<Box<dyn MediaBackend>> {
        self.loaded = false;
        self.backend.take()
    }

    /// Detach the backend from its current surface without dropping it.
    /// Called before `host.detach()` in repair paths so the engine's
    /// `Shutdown()` runs while its playback HWND is still alive
    /// (design-m2 §3.3 ordering). Without this, reattach-with-video wedges
    /// the main thread (benchmark-m2.md §7.1).
    fn detach_surface(&mut self) -> BackendResult<()> {
        if let Some(backend) = self.backend.as_mut() {
            backend.detach_surface()?;
        }
        Ok(())
    }
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take() {
            backend.shutdown();
        }
    }
}

/// Build the win → media boundary value for a host window.
fn host_surface(window: &host::HostWindow) -> HostSurface {
    // SAFETY: `hwnd` is a live window this process created; no pointers pass.
    let dpi = unsafe { GetDpiForWindow(window.hwnd) };
    HostSurface {
        hwnd: window.hwnd as isize,
        monitor: MonitorId(window.monitor.id.clone()),
        rect: window.monitor.rect,
        dpi,
    }
}

/// Attach the backend to the host window `wallpaper.set` assigned — or
/// the first window when the assignment is `None` or its monitor is gone
/// (design-m2 §5: one engine, the other monitors keep colour). Failures
/// are logged, never fatal — the desktop must stay up even when the
/// engine cannot draw.
fn attach_backend(backend: &mut BackendGuard, host: &mut Host, assigned: Option<&str>) {
    if !backend.loaded {
        return;
    }
    let window = assigned
        .and_then(|id| host.windows().iter().find(|window| window.monitor.id == id))
        .or_else(|| host.windows().first());
    let Some(surface) = window.map(host_surface) else {
        return;
    };
    // Gate experiment B (`CASCADE_GATE=B`, design-m2 §4): keep our swap
    // chain to observe the fight. Default A releases it first so the engine
    // owns the client area; the last presented pixels (the colour) stay on
    // screen meanwhile, so the desktop is never blank (AGENTS §3.1).
    let keep_presenter = std::env::var("CASCADE_GATE").is_ok_and(|value| value == "B");
    if !keep_presenter {
        host.release_presenter();
    }
    let Some(engine) = backend.backend.as_mut() else {
        return;
    };
    if let Err(err) = engine.attach_surface(surface) {
        derr!("backend: attach failed: {err}");
        // Ownership table (design-m2 §4): video failure returns the client
        // area to the colour presenter.
        host.restore_presenter();
    }
}

/// The one pause oracle's inputs (design-m4 §2.6): persisted settings +
/// runtime flags + the session lock. The lock is passed in rather than
/// read from the process-global control state so unit tests never touch
/// it; every caller passes `control::session_locked()`.
fn session_state(settings: &Settings, flags: &Flags, session_locked: bool) -> SessionState {
    SessionState {
        session_locked,
        user_paused: flags.user_paused,
        power_profile: settings.power_profile,
        pause_settings: settings.pause,
        battery_action: settings.on_battery,
        // No power collector exists before design-m4 S2b, and D8 keeps
        // the default off: S2 never reads the power source at all.
        on_battery: false,
        ..SessionState::default()
    }
}

/// Evaluate the rules and push the result into the engine. Called after
/// every state change (lock transition, pause/resume, settings, video
/// set). It always evaluates and always calls `play()`/`pause()` — both
/// are idempotent on the engine, so there is no change-detection to drift
/// out of sync (design-m4 S2).
fn apply_playback(
    backend: &mut BackendGuard,
    settings: &Settings,
    flags: &Flags,
    session_locked: bool,
) {
    if !backend.loaded {
        return;
    }
    let desired = RuleEngine::evaluate(session_state(settings, flags, session_locked));
    let Some(engine) = backend.backend.as_mut() else {
        return;
    };
    let outcome = if desired.playing {
        engine.play()
    } else {
        engine.pause()
    };
    match outcome {
        Ok(()) => dlog!(
            "playback: {}",
            if desired.playing {
                "playing".to_string()
            } else {
                desired.pause_reason.map_or_else(
                    || "paused".to_string(),
                    |reason| reason.as_str().to_string(),
                )
            }
        ),
        Err(err) => derr!("playback: {err}"),
    }
}

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::new())
}

/// Everything one command drain may touch (design-m4 §2.2). The borrow is
/// scoped to the drain: afterwards the loop publishes a fresh snapshot and
/// only then writes the responses, so a `status` asked right after
/// `pause` already sees the pause.
struct Loop<'a> {
    settings: &'a mut Settings,
    flags: &'a mut Flags,
    backend: &'a mut BackendGuard,
    host: &'a mut Host,
    surface: &'a mut ShellSurface,
    monitors: &'a [MonitorInfo],
    opts: &'a DaemonOptions,
    wallpaper: &'a WallpaperSnapshot,
    state: &'a mut DaemonState,
    store: &'a Store,
    factory: &'a BackendFactory,
}

/// Result of one drain: responses to write after the snapshot refresh,
/// plus the `quit` flush-ack handshake flag (design-m4 §2.3).
#[derive(Default)]
struct Pending {
    responses: Vec<Response>,
    quit: bool,
}

fn handle_command(ctx: &mut Loop, command: Command, pending: &mut Pending) {
    match command {
        Command::Pause { id } => {
            ctx.flags.user_paused = true;
            apply_playback(
                ctx.backend,
                ctx.settings,
                ctx.flags,
                control::session_locked(),
            );
            pending.responses.push(Response::ok(id, no_data()));
        }
        Command::Resume { id } => {
            ctx.flags.user_paused = false;
            apply_playback(
                ctx.backend,
                ctx.settings,
                ctx.flags,
                control::session_locked(),
            );
            pending.responses.push(Response::ok(id, no_data()));
        }
        Command::SetWallpaper { id, path, monitor } => {
            set_wallpaper(ctx, id, path, monitor, pending);
        }
        Command::ClearWallpaper { id, monitor } => {
            clear_wallpaper(ctx, id, monitor, pending);
        }
        Command::Restore { id } => {
            run_restore(
                ctx.host,
                ctx.surface,
                ctx.monitors,
                ctx.opts,
                ctx.wallpaper,
                ctx.backend,
                ctx.state.monitor.as_deref(),
            );
            pending.responses.push(Response::ok(id, no_data()));
        }
        Command::SettingsSet { id, patch } => {
            ctx.settings.apply_patch(patch);
            if let Err(err) = config::save(ctx.settings) {
                // Applied in memory regardless: a read-only profile dir
                // costs durability, not the setting itself.
                derr!("settings: {err}");
            }
            let mut payload = *ctx.settings;
            // Wire edge for this response (design-m4 D10): Run key, not file.
            payload.autostart = registry::autostart_state(&exe_path()).registered;
            match serde_json::to_value(payload) {
                Ok(value) => pending.responses.push(Response::ok(id, value)),
                Err(err) => pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    format!("could not build settings: {err}"),
                )),
            }
        }
        Command::Quit { id } => {
            pending.responses.push(Response::ok(id, no_data()));
            pending.quit = true;
        }
    }
}

/// `wallpaper.set` (design-m4 S2). Order matters: validate the monitor,
/// check the file, and hand the client area back to the colour presenter
/// *before* any playback surgery — every failure path then leaves the
/// desktop painted (AGENTS §3.1).
fn set_wallpaper(
    ctx: &mut Loop,
    id: u64,
    path: String,
    monitor: Option<String>,
    pending: &mut Pending,
) {
    // 1. A named monitor must exist.
    if let Some(target) = &monitor {
        if !ctx.monitors.iter().any(|info| &info.id == target) {
            pending.responses.push(Response::err(
                id,
                cascade_core::ipc::ErrorCode::NotFound,
                format!("no monitor {target}; run `cascade status` to list them"),
            ));
            return;
        }
    }
    // 2. The file must exist — checked before anything is detached.
    if !Path::new(&path).is_file() {
        pending.responses.push(Response::err(
            id,
            cascade_core::ipc::ErrorCode::FileMissing,
            format!("video file not found: {path}"),
        ));
        return;
    }
    // 3. Detach the engine while its playback HWND is still alive, then
    //    take the client area back for the colour presenter (design-m2 §4).
    if let Err(err) = ctx.backend.detach_surface() {
        derr!("set: backend detach failed: {err}");
    }
    ctx.host.restore_presenter();
    // 4. Build a backend on demand: `cascade-win` may not depend on
    //    `cascade-media` (AGENTS §4), so the binary provides the factory.
    if ctx.backend.backend.is_none() {
        match (ctx.factory)(Path::new(&path)) {
            Some(built) => ctx.backend.backend = Some(built),
            None => {
                pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    "no media backend is available; the desktop keeps the colour background",
                ));
                return;
            }
        }
    }
    // 5. Load the new source. A failure keeps the colour (step 3) and
    //    drops the broken path so the next start does not replay it.
    let failure = match ctx.backend.backend.as_mut() {
        Some(backend) => backend
            .load(Path::new(&path), &PlaybackOptions::default())
            .err()
            .map(|err| err.to_string()),
        None => Some("no media backend is available".to_string()),
    };
    if let Some(message) = failure {
        ctx.backend.loaded = false;
        ctx.state.video = None;
        ctx.state.monitor = None;
        if let Err(err) = ctx.store.save(ctx.state) {
            derr!("state: {err}");
        }
        pending.responses.push(Response::err(
            id,
            cascade_core::ipc::ErrorCode::Internal,
            format!("could not load {path}: {message}; the desktop keeps the colour background"),
        ));
        return;
    }
    ctx.backend.loaded = true;
    // 6. Persist so the next start replays this file (design-m2 §5).
    ctx.state.video = Some(path.clone());
    ctx.state.monitor = monitor;
    if let Err(err) = ctx.store.save(ctx.state) {
        derr!("state: {err}");
    }
    // 7. Attach to the assigned monitor, then apply the pause rules.
    attach_backend(ctx.backend, ctx.host, ctx.state.monitor.as_deref());
    apply_playback(
        ctx.backend,
        ctx.settings,
        ctx.flags,
        control::session_locked(),
    );
    pending.responses.push(Response::ok(id, no_data()));
}

/// `wallpaper.clear`: unload the video and hand the desktop back to the
/// colour presenter. The file itself is never touched (AGENTS §3.3).
fn clear_wallpaper(ctx: &mut Loop, id: u64, monitor: Option<String>, pending: &mut Pending) {
    if let Some(target) = &monitor {
        if !ctx.monitors.iter().any(|info| &info.id == target) {
            pending.responses.push(Response::err(
                id,
                cascade_core::ipc::ErrorCode::NotFound,
                format!("no monitor {target}; run `cascade status` to list them"),
            ));
            return;
        }
    }
    if let Err(err) = ctx.backend.detach_surface() {
        derr!("clear: backend detach failed: {err}");
    }
    ctx.backend.loaded = false;
    ctx.host.restore_presenter();
    ctx.state.video = None;
    ctx.state.monitor = None;
    if let Err(err) = ctx.store.save(ctx.state) {
        derr!("state: {err}");
    }
    pending.responses.push(Response::ok(id, no_data()));
}

/// The restore ladder: detach, repaint, reattach. Shared by the window
/// message (`cascade restore` fallback) and the `wallpaper.restore`
/// command so both behave identically.
fn run_restore(
    host: &mut Host,
    surface: &mut ShellSurface,
    monitors: &[MonitorInfo],
    opts: &DaemonOptions,
    wallpaper: &WallpaperSnapshot,
    backend: &mut BackendGuard,
    assigned: Option<&str>,
) {
    dlog!("restore requested");
    if let Err(err) = backend.detach_surface() {
        derr!("restore: backend detach failed: {err}");
    }
    host.detach();
    let report = repaint::ladder(surface, wallpaper, opts.repaint_level);
    print_repaint("restore", &report);
    match reattach(monitors, opts) {
        Some((new_surface, new_host)) => {
            *surface = new_surface;
            *host = new_host;
            print_attach(host);
            attach_backend(backend, host, assigned);
        }
        None => {
            derr!("restore: Explorer did not return; the watchdog will keep trying");
        }
    }
}

/// Run the daemon until asked to stop. Always repaints the desktop before
/// returning, including on the restore path.
pub fn run(
    opts: DaemonOptions,
    backend: Option<Box<dyn MediaBackend>>,
    factory: BackendFactory,
) -> Result<(), DaemonError> {
    // Declared first, therefore dropped last on every exit path (`Ok`, an
    // early `Err`, and panic unwinding): only then is `EXIT_DONE` signalled,
    // which lets a waiting console-control handler return — and Windows only
    // kills the process after that return.
    let _exit = ExitSignal;
    // Declared second, therefore dropped before `_exit`: an early return
    // still shuts the engine down, and the normal exit path takes it out
    // explicitly first (design-m2 §3.3 ordering: shutdown → detach → repaint).
    let mut backend_guard = BackendGuard {
        backend,
        loaded: false,
    };

    // The pipe is the primary single-instance lock (`ipc.md` §1): the first
    // daemon creates it with `FILE_FLAG_FIRST_PIPE_INSTANCE`, a second one
    // fails here. The control window check stays as the fallback for an
    // older daemon that has no pipe yet. On any failure `pipe` drops and
    // closes the handle, so nothing is left behind.
    let pipe = match PipeServer::create() {
        Ok(pipe) => pipe,
        Err(code) => {
            if let Some(existing) = control::find_existing() {
                return Err(DaemonError::AlreadyRunning(existing as usize));
            }
            return Err(DaemonError::Pipe(code));
        }
    };
    if let Some(existing) = control::find_existing() {
        return Err(DaemonError::AlreadyRunning(existing as usize));
    }

    install_panic_hook();

    let store = Store::open();
    let previous = store.load();
    // Persisted settings (design-m4 D10): a missing or corrupt
    // `config.toml` decodes to the documented defaults and never blocks.
    let mut settings = config::load();
    // Runtime-only flags: a manual pause lives for this session only.
    let mut flags = Flags::default();

    let control = ControlWindow::create().map_err(DaemonError::Control)?;
    CONTROL_HWND.store(control.hwnd() as isize, Ordering::Release);
    // SAFETY: handler stays valid for the process lifetime; failure only means
    // there is no console attached (the autostart case).
    unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) };

    // M3 slice: session lock pauses playback (design-m3 §3). A failure only
    // disables pause-on-lock; the daemon must run either way.
    if let Err(code) = control::register_session_notifications(control.hwnd()) {
        derr!("session notifications unavailable (error {code}); pause-on-lock disabled");
    }

    let snapshot = crate::wallpaper::snapshot();
    dlog!("cascade {} starting", env!("CARGO_PKG_VERSION"));
    // The executable path makes bug reports unambiguous: it shows exactly
    // which build is running and from where (install dir vs. a temp copy).
    if let Ok(exe) = std::env::current_exe() {
        dlog!("  exe: {}", exe.display());
    }
    dlog!(
        "  wallpaper snapshot: {}",
        snapshot.path.as_deref().unwrap_or("(none)")
    );
    if let Some(previous) = &previous {
        dlog!(
            "  previous state: running={} pid={} updated_unix={}",
            previous.running,
            previous.pid,
            previous.updated_unix
        );
    }
    dlog!(
        "  settings: power_profile={} on_battery={} pause.session_lock={}",
        settings.power_profile.as_str(),
        settings.on_battery.as_str(),
        settings.pause.session_lock
    );

    // Resolve the video source before anything is drawn: `--video` wins,
    // otherwise the last successful path replays (design-m2 §5). A load
    // failure degrades to the colour background — never a blank desktop
    // (AGENTS §3.1) — and does not persist the broken path.
    let video = opts.video.clone().or_else(|| {
        previous
            .as_ref()
            .and_then(|state| state.video.as_ref())
            .map(PathBuf::from)
    });
    // The monitor the last `wallpaper.set` targeted (design-m4 §5.4);
    // `attach_backend` falls back to the first window if it is gone.
    let assigned = previous.as_ref().and_then(|state| state.monitor.clone());
    if let Some(source) = &video {
        match backend_guard.backend.as_mut() {
            Some(backend) => match backend.load(source, &PlaybackOptions::default()) {
                Ok(()) => {
                    dlog!("  video: {}", source.display());
                    backend_guard.loaded = true;
                }
                Err(err) => derr!("video: {err}; continuing with the colour background"),
            },
            None => derr!(
                "video: {} requested but no backend was built",
                source.display()
            ),
        }
    }

    let mut surface = shell::discover(opts.shell_timeout).map_err(DaemonError::Shell)?;

    // A previous session that died without a clean exit can leave Explorer
    // with a stale desktop composition. Repaint once before we draw on it.
    if previous.as_ref().is_some_and(|state| state.running) {
        dlog!("  previous session did not stop cleanly; repainting the desktop first");
        let report = repaint::ladder(&surface, &snapshot, 1);
        print_repaint("recover", &report);
    }

    let mut monitors = monitor::enumerate();
    if monitors.is_empty() {
        return Err(DaemonError::NoMonitors);
    }

    let mut host = attach(surface, &monitors, &opts).map_err(DaemonError::Host)?;
    print_attach(&host);
    attach_backend(&mut backend_guard, &mut host, assigned.as_deref());
    // Apply the pause rules to the just-attached engine: a session that
    // starts locked must not start playing (design-m4 §2.6).
    apply_playback(
        &mut backend_guard,
        &settings,
        &flags,
        control::session_locked(),
    );

    // Only a source that actually loaded is persisted (design-m2 §5): a
    // failed path must not come back on the next start.
    let persisted_video = backend_guard
        .loaded
        .then_some(video)
        .flatten()
        .map(|path| path.display().to_string());
    let mut state = DaemonState::running_now(
        std::process::id(),
        host::rgb_hex(opts.color),
        opts.repaint_level,
        opts.host_mode.label().to_string(),
        persisted_video,
        assigned,
    );
    if let Err(err) = store.save(&state) {
        derr!("state: {err}");
    }
    if let Some(path) = store.path() {
        dlog!("  state: {}", path.display());
    }
    dlog!(
        "  Ctrl+C in this console, `cascade stop`, or `cascade quit` restores the wallpaper and quits"
    );

    // The render loop publishes state to the pipe thread through one
    // message channel (snapshots and command responses, design-m4 §2.2);
    // commands travel the other way and wake the loop with `WM_APP_PIPE`.
    // Send the first snapshot before spawning so an early `status` never
    // sees the `boot` fallback.
    let (msg_tx, msg_rx) = mpsc::channel::<ServerMsg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (flush_tx, flush_rx) = mpsc::channel::<()>();
    let first = Arc::new(current_snapshot(&host, &monitors, &opts, &settings, &flags));
    if msg_tx
        .send(ServerMsg::Snapshot(Arc::clone(&first)))
        .is_err()
    {
        derr!("ipc: message channel rejected the first send; status stays at boot");
    }
    let mut published = Some(first);
    if let Err(code) = pipe.serve(PipeChannels {
        messages: msg_rx,
        commands: cmd_tx,
        quit_flushed: flush_tx,
    }) {
        // The desktop must stay up regardless: log and continue without a
        // control channel (the control-window fallback still detects a
        // second daemon).
        derr!("ipc: could not start the pipe thread (error {code}); no `cascade status`");
    }

    let mut msg = MSG::default();
    while !control::quit_requested() {
        // Commands from the pipe thread first (design-m4 §2.2): apply
        // every queued mutation, publish the resulting snapshot, and only
        // then write the responses — a client that asks `status` right
        // after `pause` must already see the pause.
        let pending = {
            let mut ctx = Loop {
                settings: &mut settings,
                flags: &mut flags,
                backend: &mut backend_guard,
                host: &mut host,
                surface: &mut surface,
                monitors: &monitors,
                opts: &opts,
                wallpaper: &snapshot,
                state: &mut state,
                store: &store,
                factory: &factory,
            };
            let mut pending = Pending::default();
            while let Ok(command) = cmd_rx.try_recv() {
                handle_command(&mut ctx, command, &mut pending);
            }
            pending
        };
        let Pending { responses, quit } = pending;
        if !responses.is_empty() {
            refresh_status(
                &msg_tx,
                &mut published,
                &host,
                &monitors,
                &opts,
                &settings,
                &flags,
            );
            for response in responses {
                if msg_tx.send(ServerMsg::Response(response)).is_err() {
                    derr!("ipc: response channel closed; the client will time out");
                }
            }
        }
        if quit {
            // The pipe thread flushes its write to the client, then acks;
            // only then does the exit ladder start (design-m4 §2.3). A
            // missing ack must not keep the daemon alive forever.
            if let Err(err) = flush_rx.recv_timeout(QUIT_FLUSH_WAIT) {
                derr!("ipc: quit flush ack missing ({err}); quitting anyway");
            }
            control::request_quit();
        }
        if control::quit_requested() {
            break;
        }

        // SAFETY: standard Win32 message pump on this thread.
        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(
                0,
                std::ptr::null(),
                WATCHDOG_TICK.as_millis() as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if wait == WAIT_TIMEOUT {
            watchdog_tick(
                &mut host,
                &mut surface,
                &monitors,
                &opts,
                &snapshot,
                &mut backend_guard,
                state.monitor.as_deref(),
            );
            refresh_status(
                &msg_tx,
                &mut published,
                &host,
                &monitors,
                &opts,
                &settings,
                &flags,
            );
            continue;
        }
        if wait != WAIT_OBJECT_0 {
            break;
        }

        // Drain everything that is queued so a busy queue cannot starve the
        // watchdog or block a stop request.
        loop {
            // SAFETY: `msg` is a valid out-parameter; filters mean "any".
            let has_message =
                unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
            if has_message == 0 {
                break;
            }
            if msg.message == WM_QUIT {
                break;
            }
            // SAFETY: `msg` was filled by PeekMessageW.
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        if control::take_restore_request() {
            run_restore(
                &mut host,
                &mut surface,
                &monitors,
                &opts,
                &snapshot,
                &mut backend_guard,
                state.monitor.as_deref(),
            );
        }

        if control::take_session_lock().is_some() {
            // The transition only says "re-evaluate"; the current state
            // comes from the control window itself.
            apply_playback(
                &mut backend_guard,
                &settings,
                &flags,
                control::session_locked(),
            );
        }

        if control::take_display_change() {
            dlog!("display change; re-reading monitors");
            monitors = monitor::enumerate();
            if !monitors.is_empty() {
                if let Err(err) = backend_guard.detach_surface() {
                    derr!("display change: backend detach failed: {err}");
                }
                host.detach();
                match reattach(&monitors, &opts) {
                    Some((new_surface, new_host)) => {
                        surface = new_surface;
                        host = new_host;
                        print_attach(&host);
                        attach_backend(&mut backend_guard, &mut host, state.monitor.as_deref());
                    }
                    None => {
                        derr!("display change: reattach failed; the watchdog will keep trying");
                    }
                }
            }
        }

        // Every message-driven state change (restore, lock, display) is
        // published here; the command drain and the watchdog publish theirs.
        refresh_status(
            &msg_tx,
            &mut published,
            &host,
            &monitors,
            &opts,
            &settings,
            &flags,
        );
    }

    // Critical work first, prints after: a console close (CTRL_CLOSE) can
    // kill this process as soon as the control handler returns, and that
    // handler waits for `ExitSignal` — which drops at the end of this
    // function. The desktop must be repainted and the state saved before
    // anything else, including logging.
    dlog!("stopping");
    // design-m2 §3.3: the engine releases its playback window first, then
    // the host dies, then the repaint ladder runs.
    if let Some(backend) = backend_guard.take() {
        backend.shutdown();
    }
    CONTROL_HWND.store(0, Ordering::Release);
    host.detach();
    let report = repaint::ladder(&surface, &snapshot, opts.repaint_level);

    state.running = false;
    let saved = store.save(&state);

    print_repaint("exit", &report);
    if let Err(err) = saved {
        derr!("state: {err}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_core::rule_engine::PauseSettings;
    use cascade_core::BackendStats;

    /// Records pause/play calls so `apply_session_lock` can be asserted
    /// without a real engine (no Win32 in tests).
    struct PauseMock {
        pauses: std::sync::Arc<AtomicU32>,
        plays: std::sync::Arc<AtomicU32>,
    }

    impl MediaBackend for PauseMock {
        fn load(
            &mut self,
            _source: &std::path::Path,
            _opts: &PlaybackOptions,
        ) -> BackendResult<()> {
            Ok(())
        }
        fn play(&mut self) -> BackendResult<()> {
            self.plays.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn pause(&mut self) -> BackendResult<()> {
            self.pauses.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn set_looping(&mut self, _on: bool) -> BackendResult<()> {
            Ok(())
        }
        fn stats(&self) -> BackendStats {
            BackendStats::default()
        }
        fn attach_surface(&mut self, _surface: HostSurface) -> BackendResult<()> {
            Ok(())
        }
        fn detach_surface(&mut self) -> BackendResult<()> {
            Ok(())
        }
        fn shutdown(self: Box<Self>) {}
    }

    fn mock_backend() -> (
        PauseMock,
        std::sync::Arc<AtomicU32>,
        std::sync::Arc<AtomicU32>,
    ) {
        let pauses = std::sync::Arc::new(AtomicU32::new(0));
        let plays = std::sync::Arc::new(AtomicU32::new(0));
        let mock = PauseMock {
            pauses: pauses.clone(),
            plays: plays.clone(),
        };
        (mock, pauses, plays)
    }

    #[test]
    fn user_pause_pauses_and_resume_plays() {
        let (mock, pauses, plays) = mock_backend();
        let mut backend = BackendGuard {
            backend: Some(Box::new(mock)),
            loaded: true,
        };
        let settings = Settings::default();
        let mut flags = Flags { user_paused: true };

        apply_playback(&mut backend, &settings, &flags, false);
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
        assert_eq!(plays.load(Ordering::SeqCst), 0);

        flags.user_paused = false;
        apply_playback(&mut backend, &settings, &flags, false);
        assert_eq!(plays.load(Ordering::SeqCst), 1);
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session_lock_pauses_unless_the_toggle_is_off() {
        let (mock, pauses, plays) = mock_backend();
        let mut backend = BackendGuard {
            backend: Some(Box::new(mock)),
            loaded: true,
        };
        let flags = Flags::default();

        // Documented default: `pause.session_lock = true` (ipc.md §5).
        apply_playback(&mut backend, &Settings::default(), &flags, true);
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
        assert_eq!(plays.load(Ordering::SeqCst), 0);

        // The S2 settings toggle is honoured (design-m4 §2.6): with the
        // toggle off a locked session plays instead of pausing.
        let relaxed = Settings {
            pause: PauseSettings {
                session_lock: false,
                ..PauseSettings::default()
            },
            ..Settings::default()
        };
        apply_playback(&mut backend, &relaxed, &flags, true);
        assert_eq!(plays.load(Ordering::SeqCst), 1, "lock must not pause");
        assert_eq!(pauses.load(Ordering::SeqCst), 1, "no extra pause");
    }

    #[test]
    fn apply_playback_without_a_video_is_a_no_op() {
        // Backend present but no video loaded: the rule must not reach it.
        let (mock, pauses, plays) = mock_backend();
        let mut not_loaded = BackendGuard {
            backend: Some(Box::new(mock)),
            loaded: false,
        };
        apply_playback(
            &mut not_loaded,
            &Settings::default(),
            &Flags::default(),
            false,
        );
        assert_eq!(pauses.load(Ordering::SeqCst), 0);
        assert_eq!(plays.load(Ordering::SeqCst), 0);

        // No backend box at all: must not panic.
        let mut no_backend = BackendGuard {
            backend: None,
            loaded: true,
        };
        apply_playback(
            &mut no_backend,
            &Settings::default(),
            &Flags::default(),
            true,
        );
    }

    #[test]
    fn session_state_maps_settings_flags_and_lock() {
        let settings = Settings {
            pause: PauseSettings {
                maximized: true,
                ..PauseSettings::default()
            },
            ..Settings::default()
        };
        let mut flags = Flags::default();

        let state = session_state(&settings, &flags, false);
        assert!(!state.session_locked);
        assert!(!state.user_paused);
        assert!(
            state.pause_settings.maximized,
            "settings flow into the engine"
        );
        assert!(
            matches!(
                state.battery_action,
                cascade_core::rule_engine::BatteryAction::Off
            ),
            "D8 default: off"
        );
        assert!(
            !state.on_battery,
            "no power collector exists before design-m4 S2b"
        );

        flags.user_paused = true;
        let state = session_state(&settings, &flags, true);
        assert!(state.user_paused, "runtime flag wins over defaults");
        assert!(state.session_locked);
    }

    #[test]
    fn error_messages_tell_the_user_what_to_do() {
        assert_eq!(
            DaemonError::AlreadyRunning(0xABCD).to_string(),
            "Cascade is already running (control window 0xabcd); use `cascade restore` or `cascade stop`"
        );
        assert_eq!(
            DaemonError::Control(5).to_string(),
            "could not create the daemon window (error 5)"
        );
        assert_eq!(
            DaemonError::NoMonitors.to_string(),
            "no monitors were reported by the system"
        );
    }

    #[test]
    fn default_options_match_the_documented_baseline() {
        // docs/architecture.md: level 1, 60 s shell discovery, teal, auto.
        let options = DaemonOptions::default();
        assert_eq!(options.repaint_level, 1);
        assert_eq!(options.shell_timeout, Duration::from_secs(60));
        assert_eq!(options.host_mode, host::HostModePreference::Auto);
        assert_eq!(options.color, host::HostOptions::default().color);
        // Slice 2: no `--video`, no persisted path -> no backend source.
        assert!(options.video.is_none());
    }
}
