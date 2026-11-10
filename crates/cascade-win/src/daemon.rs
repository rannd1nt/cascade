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

use cascade_core::assess::{assess, recommend, Assessment};
use cascade_core::device::{DeviceProfile, MonitorProfile};
use cascade_core::ipc::{
    no_data, AssessReport, DeviceSummary, ItemAssessment, LibraryAddResult, LibraryItemsData,
    LibraryPruneResult, LibraryRef, MonitorSummary, RejectedPath, Response, SetReport, Settings,
    VideoSize,
};
use cascade_core::library::{Library, LibraryItem, VideoMetadata};
use cascade_core::rule_engine::{BatteryAction, RuleEngine, SessionState};
use cascade_core::{
    BackendResult, Fit, HostSurface, MediaBackend, MonitorId, PlaybackOptions, PresentationMode,
};
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
use crate::ipc::{
    Command, PipeChannels, PipeServer, PlaybackStatus, ServerMsg, StatusSnapshot, QUIT_FLUSH_WAIT,
};
use crate::library::LibraryStore;
use crate::monitor::{self, MonitorInfo};
use crate::power;
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
    /// Confirmed power source from the S2b collector (design-m4 §2.6).
    /// Runtime-only, and it stays `false` while the collector is gated
    /// off (D8: `on_battery == off` means no read, no subscription).
    on_battery: bool,
}

/// Builds a media backend on demand — the first `wallpaper.set` on a
/// daemon that started without a video needs one. `cascade-win` may not
/// depend on `cascade-media` (AGENTS §4), so the binary passes this
/// factory in. The path lets the binary choose per source later; `None`
/// means no backend is available and the desktop keeps its colour.
pub type BackendFactory = Box<dyn Fn(&Path) -> Option<Box<dyn MediaBackend>> + Send>;
/// Metadata probe injected by the binary (design-m4 §4), the same way the
/// backend factory is: `cascade-win` may not depend on `cascade-media`
/// (AGENTS §4), and the library entry type lives in `cascade-core`.
pub type ProbeFn = Box<dyn Fn(&Path) -> Result<VideoMetadata, String> + Send>;

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
    let mut attempt = 0;
    while started.elapsed() < RECOVERY_WINDOW {
        attempt += 1;
        derr!(
            "[wd] p12: reattach attempt {attempt} (elapsed {:?})",
            started.elapsed()
        );
        match shell::discover(Duration::from_millis(750)) {
            Ok(surface) => {
                derr!("[wd] p13: discover ok, attaching");
                match attach(surface, monitors, opts) {
                    Ok(host) => {
                        derr!("[wd] p14: attach ok");
                        return Some((surface, host));
                    }
                    Err(err) => derr!("reattach: {err}"),
                }
            }
            Err(err) => derr!("reattach: {err}"),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    derr!("[wd] p15: RECOVERY_WINDOW exhausted, returning None");
    None
}

/// The `monitors[]` playback row for the monitor the engine is drawing on
/// (`ipc.md` §4, M4 S3b).
///
/// `None` unless a video is loaded **and** currently attached: a detached
/// backend has no host window to name, and a colour-only desktop has no
/// stats to report. The source is matched against the library with
/// `find_by_path` (canonical, case-insensitive), so a path that was never
/// added simply carries no `source`.
fn playback_status(
    backend: &BackendGuard,
    host: &Host,
    state: &DaemonState,
    library: &Library,
) -> Option<PlaybackStatus> {
    if !backend.loaded {
        return None;
    }
    let hwnd = backend.attached_hwnd?;
    let engine = backend.backend.as_ref()?;
    let monitor = host
        .windows()
        .iter()
        .find(|window| window.hwnd as isize == hwnd)?
        .monitor
        .id
        .clone();
    let stats = engine.stats();
    let source = state
        .video
        .as_deref()
        .and_then(|path| library.find_by_path(path))
        .map(|item| LibraryRef {
            id: item.id.clone(),
            title: item.title.clone(),
        });
    Some(PlaybackStatus {
        monitor,
        source,
        hwdec: stats.hardware_decode,
        // A backend reporting 0 fps or a 0×0 size is saying "unknown";
        // the field is omitted instead of showing a fake zero.
        fps: (stats.fps > 0.0).then_some(stats.fps),
        dropped: Some(stats.dropped_frames),
        video: (stats.video_size != (0, 0)).then_some(VideoSize {
            w: stats.video_size.0,
            h: stats.video_size.1,
        }),
        engine: stats.engine_mode,
    })
}

/// Everything `refresh_status` reads off the render loop. Grouped into one
/// value because the playback row (M4 S3b) added the backend, the
/// persisted state, and the library — ten loose parameters would sit well
/// past clippy's limit and read worse than this name does.
struct StatusInputs<'a> {
    host: &'a Host,
    monitors: &'a [MonitorInfo],
    opts: &'a DaemonOptions,
    settings: &'a Settings,
    flags: &'a Flags,
    backend: &'a BackendGuard,
    state: &'a DaemonState,
    library: &'a Library,
}

/// Build the render-loop half of a `status` snapshot from current state.
/// Mode and reasons come from the one pause oracle (design-m4 §2.6);
/// memory and autostart are read on demand by the pipe thread instead
/// (`design-m4` §2.2), so this stays cheap enough for the 2 s watchdog.
fn current_snapshot(inputs: &StatusInputs) -> StatusSnapshot {
    let desired = RuleEngine::evaluate(session_state(
        inputs.settings,
        inputs.flags,
        control::session_locked(),
    ));
    // `!playing` outranks host health: a paused engine on an unhealthy
    // host is still "paused" — the watchdog reports health separately.
    let mode = if !desired.playing {
        "paused"
    } else if inputs.host.is_healthy() {
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
        monitors: inputs.monitors.to_vec(),
        playback: playback_status(inputs.backend, inputs.host, inputs.state, inputs.library),
        host_strategy: inputs.host.surface().mode.label().to_string(),
        repaint_level: inputs.opts.repaint_level,
        settings: *inputs.settings,
    }
}

/// Publish the snapshot to the pipe thread only when it changed, so the
/// channel carries no traffic while the desktop is idle. While a video
/// plays the live counters (`fps`, `dropped`) can differ on every tick —
/// that costs at most one small message per watchdog tick (1–2 s), which
/// is the price of `status` being current.
fn refresh_status(
    tx: &mpsc::Sender<ServerMsg>,
    published: &mut Option<Arc<StatusSnapshot>>,
    inputs: &StatusInputs<'_>,
) {
    let next = Arc::new(current_snapshot(inputs));
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
    // `[wd]` probes: every silent step of this repair path carries one, so a
    // wedge stops on the exact step instead of between two log lines. The
    // write is unbuffered (see `derr!`) and therefore on disk before a hang.
    derr!("[wd] p1: entering surface_is_alive/restack");
    let alive_before = shell::surface_is_alive(surface);
    derr!("[wd] p2: surface_is_alive={alive_before}");

    // Cheap repair first: another window may simply have been raised above us.
    if alive_before {
        derr!("[wd] p3: restacking");
        host.restack();
        derr!("[wd] p4: restack returned, healthy={}", host.is_healthy());
        if host.is_healthy() {
            dlog!("watchdog: z-order restored");
            return true;
        }
    }

    // Detach backend before destroying host windows so engine Shutdown()
    // runs while its playback HWND is still alive (design-m2 §3.3).
    derr!("[wd] p5: before backend.detach_surface()");
    if let Err(err) = backend.detach_surface() {
        derr!("watchdog: backend detach failed: {err}");
    }
    derr!("[wd] p6: backend.detach_surface() returned");

    derr!("[wd] p7: before host.detach()");
    host.detach();
    derr!("[wd] p8: host.detach() returned");
    if shell::surface_is_alive(surface) {
        derr!("[wd] p9: before repaint ladder");
        let report = repaint::ladder(surface, snapshot, 1);
        derr!("[wd] p10: repaint ladder returned");
        print_repaint("watchdog", &report);
    } else {
        derr!("[wd] p9: surface dead, repaint skipped");
    }

    derr!("[wd] p11: before reattach()");
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
    /// HWND the engine is currently bound to, set only after a successful
    /// `attach_surface` and cleared by every detach/take. `wallpaper.set`
    /// uses it to recognise "already playing this file on this window"
    /// and skip the load entirely (design-m2 §4).
    attached_hwnd: Option<isize>,
    /// Successful `load`s in this process (the startup load counts as 1).
    /// Every load pipeline retains ~50 MB until process exit
    /// (design-m4 §8.3), so once this is ≥ 1 a different-file set restarts
    /// the daemon instead of loading again (design-m2 §4).
    session_loads: u32,
}

impl BackendGuard {
    fn take(&mut self) -> Option<Box<dyn MediaBackend>> {
        self.loaded = false;
        self.attached_hwnd = None;
        self.backend.take()
    }

    /// Detach the backend from its current surface without dropping it.
    /// Called before `host.detach()` in repair paths so the engine's
    /// `Shutdown()` runs while its playback HWND is still alive
    /// (design-m2 §3.3 ordering). Without this, reattach-with-video wedges
    /// the main thread (benchmark-m2.md §7.1).
    fn detach_surface(&mut self) -> BackendResult<()> {
        self.attached_hwnd = None;
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
    let hwnd = surface.hwnd;
    if let Err(err) = engine.attach_surface(surface) {
        derr!("backend: attach failed: {err}");
        // Ownership table (design-m2 §4): video failure returns the client
        // area to the colour presenter.
        host.restore_presenter();
    } else {
        // Reuse bookkeeping (design-m2 §4 reload amendment): only a
        // successful attach makes the engine eligible for the same-HWND
        // reload path in `set_wallpaper`.
        backend.attached_hwnd = Some(hwnd);
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
        // The collector's confirmed state (S2b); `false` while the gate is
        // off, which is exactly what D8 promises: no read at the default.
        on_battery: flags.on_battery,
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

/// Everything one `power_step` may touch (design-m4 §2.6). Grouped into
/// one value for the same reason as `StatusInputs`: nine loose parameters
/// would sit well past clippy's limit.
struct PowerStep<'a> {
    /// Control window that receives `WM_POWERBROADCAST`.
    hwnd: HWND,
    settings: &'a Settings,
    flags: &'a mut Flags,
    backend: &'a mut BackendGuard,
    host: &'a mut Host,
    /// Read-only: only `state.video.is_some()` matters here — an unload
    /// keeps the file reference (AGENTS §3.3) so a later respawn reloads it.
    state: &'a DaemonState,
    registration: &'a mut Option<isize>,
    hysteresis: &'a mut power::Hysteresis,
    collector_on: &'a mut bool,
}

/// One turn of the power collector (design-m4 §2.6, D8/D9), run on every
/// main-loop wake — the watchdog timeout is its only tick, so there is no
/// timer and no per-tick hardware read:
///
/// 1. **Gate** — subscribe only while `on_battery != off` (D8: at the
///    default the daemon never reads the power source). One seed read on
///    enable, unregister + reset on disable.
/// 2. **Sample** — feed the raw broadcast through the 3 s [`power::Hysteresis`]
///    into `flags.on_battery`, the value the pause oracle sees.
/// 3. **Reconcile** — [`power::reconcile`] decides unload vs. respawn vs.
///    nothing; a state change re-evaluates the rules (they are idempotent).
///
/// Returns `true` when the daemon must respawn to bring the video back:
/// this process has already loaded once, and a second in-process load
/// would retain another ~50 MB pipeline (§8.3), so the reload always runs
/// in a fresh process.
fn power_step(step: PowerStep<'_>) -> bool {
    let PowerStep {
        hwnd,
        settings,
        flags,
        backend,
        host,
        state,
        registration,
        hysteresis,
        collector_on,
    } = step;
    let action = settings.on_battery;
    let mut changed = false;

    // 1. The D8 gate.
    let want = action != BatteryAction::Off;
    if want && !*collector_on {
        *collector_on = true;
        power::log_gate(true, action);
        if let Some(raw) = power::read_source() {
            hysteresis.observe(raw, Instant::now());
        }
        match
            // SAFETY: `hwnd` is our control window, alive on this thread
            // for the whole loop; it is unregistered on the exit ladder.
            unsafe { power::register(hwnd) }
        {
            Ok(handle) => *registration = Some(handle),
            // Logged once; the next off→on toggle of the setting retries.
            Err(code) => power::log_register_failure(code),
        }
    } else if !want && *collector_on {
        *collector_on = false;
        power::log_gate(false, action);
        if let Some(handle) = registration.take() {
            power::unregister(handle);
        }
        // Drop any broadcast recorded between the last wake and the
        // unregister: it must not seed the next enable with a stale sample.
        let _ = power::take_raw();
        hysteresis.reset();
        if flags.on_battery {
            flags.on_battery = false;
            changed = true;
        }
    }

    // 2. Raw sample → hysteresis → confirmed state. A broadcast is only
    //    consumed while subscribed; a stale one is dropped by the disable
    //    path's `reset` above.
    if *collector_on {
        if let Some(raw) = power::take_raw() {
            hysteresis.observe(raw, Instant::now());
        }
        let confirmed = hysteresis.tick(Instant::now());
        if confirmed != flags.on_battery {
            dlog!(
                "power: source is now {}",
                if confirmed { "battery" } else { "AC" }
            );
            flags.on_battery = confirmed;
            changed = true;
        }
    }

    // 3. Reconcile the engine with the confirmed state.
    match power::reconcile(
        flags.on_battery,
        action,
        backend.loaded,
        state.video.is_some(),
    ) {
        power::Reconcile::Reload => return true,
        power::Reconcile::Unload => {
            // `on_battery=stop` on battery with a loaded engine (design-m4
            // §2.6): the engine lets go of its playback window, the colour
            // presenter takes the client area back — the desktop is never
            // blank (AGENTS §3.1) and the file is never touched (§3.3).
            // `state.video` is kept, so an AC return respawns with it.
            if let Err(err) = backend.detach_surface() {
                derr!("power: backend detach failed: {err}");
            }
            backend.loaded = false;
            host.restore_presenter();
            dlog!("power: on_battery=stop unloaded the video; the desktop keeps its colour");
            return false;
        }
        power::Reconcile::Nothing => {}
    }
    if changed {
        apply_playback(backend, settings, flags, control::session_locked());
    }
    false
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
    /// In-memory library (design-m4 §4): the main loop is its single
    /// writer, so the pipe thread never sees it.
    library: &'a mut Library,
    lib_store: &'a LibraryStore,
    probe: &'a ProbeFn,
}

/// Result of one drain: responses to write after the snapshot refresh,
/// plus the `quit` flush-ack handshake flag (design-m4 §2.3) and the
/// restart decision for a different-file `wallpaper.set` (design-m2 §4).
#[derive(Default)]
struct Pending {
    responses: Vec<Response>,
    quit: bool,
    respawn: Option<u64>,
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
            let previous_mode = ctx.settings.engine_mode;
            ctx.settings.apply_patch(patch);
            if let Err(err) = config::save(ctx.settings) {
                // Applied in memory regardless: a read-only profile dir
                // costs durability, not the setting itself.
                derr!("settings: {err}");
            }
            // Settings are state (design-m4 §2.6): re-evaluate the oracle
            // so a rule toggle (`on_battery`, `pause.*`) takes effect now,
            // not at the next unrelated event. Idempotent like every other
            // `apply_playback` call site.
            apply_playback(
                ctx.backend,
                ctx.settings,
                ctx.flags,
                control::session_locked(),
            );
            // AGENTS §3.15 + `design-engine-mode.md` §3: a changed
            // `engine_mode` means the engine must be *built* differently,
            // and building it is a load pipeline — every load retains ≈50 MB
            // until process exit (`design-m4.md` §8.3, the bug that made
            // every `wallpaper.set` restart). So: persist as plain state,
            // answer the client, then the exit ladder respawns and the child
            // reads config.toml and builds the engine exactly once, in the
            // requested mode. No in-process rebuild is ever attempted.
            let action =
                engine_mode_action(previous_mode, ctx.settings.engine_mode, ctx.backend.loaded);
            let mut payload = *ctx.settings;
            // Wire edge for this response (design-m4 D10): Run key, not file.
            payload.autostart = registry::autostart_state(&exe_path()).registered;
            let mut response = match serde_json::to_value(payload) {
                Ok(value) => Response::ok(id, value),
                Err(err) => Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    format!("could not build settings: {err}"),
                ),
            };
            match action {
                PresentationModeAction::Restart => {
                    // The client gets this answer first; the flush ack then
                    // runs the exit ladder — the same handshake as a
                    // `wallpaper.set` restart (design-m2 §4).
                    response.flush_after = true;
                    dlog!(
                        "settings: engine_mode → {}; restarting to build the engine in that mode",
                        ctx.settings.engine_mode.as_str()
                    );
                }
                PresentationModeAction::PersistOnly => dlog!(
                    "settings: engine_mode → {}; no engine loaded, applied at the next load",
                    ctx.settings.engine_mode.as_str()
                ),
                PresentationModeAction::Keep => {}
            }
            pending.responses.push(response);
            if action == PresentationModeAction::Restart {
                pending.respawn = Some(id);
            }
        }
        Command::Quit { id } => {
            pending.responses.push(Response::ok(id, no_data()));
            pending.quit = true;
        }
        Command::LibraryList { id } => {
            let data = LibraryItemsData {
                items: ctx.library.items.clone(),
            };
            match serde_json::to_value(data) {
                Ok(value) => pending.responses.push(Response::ok(id, value)),
                Err(err) => pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    format!("could not build the library list: {err}"),
                )),
            }
        }
        Command::LibraryAdd { id, paths } => {
            let result = library_add(ctx, paths);
            persist_library(ctx);
            match serde_json::to_value(result) {
                Ok(value) => pending.responses.push(Response::ok(id, value)),
                Err(err) => pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    format!("could not build the add result: {err}"),
                )),
            }
        }
        Command::LibraryRemove { id, item_id } => match ctx.library.remove(&item_id) {
            Some(_) => {
                persist_library(ctx);
                pending.responses.push(Response::ok(id, no_data()));
            }
            None => pending.responses.push(Response::err(
                id,
                cascade_core::ipc::ErrorCode::NotFound,
                format!("no library item with id '{item_id}' — run `cascade library list`"),
            )),
        },
        Command::LibraryPrune { id } => {
            let removed = ctx.library.prune(|path| Path::new(path).is_file());
            if removed > 0 {
                dlog!("library: pruned {removed} entries with missing files");
                persist_library(ctx);
            }
            match serde_json::to_value(LibraryPruneResult {
                removed: removed as u32,
            }) {
                Ok(value) => pending.responses.push(Response::ok(id, value)),
                Err(err) => pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    format!("could not build the prune result: {err}"),
                )),
            }
        }
        Command::SetWallpaperById {
            id,
            item_id,
            monitor,
        } => {
            // Resolve first, then reuse the validated path flow — the
            // monitor/file checks in `set_wallpaper` still run.
            match ctx.library.find(&item_id).map(|item| item.path.clone()) {
                Some(path) => set_wallpaper(ctx, id, path, monitor, pending),
                None => pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::NotFound,
                    format!("no library item with id '{item_id}' — run `cascade library list`"),
                )),
            }
        }
        Command::Assess { id, path, monitor } => {
            // Read-only diagnosis (design-m4 §3/§5): collect the device
            // fresh (D13 — no cached profile can go stale), resolve the
            // descriptor probe-once, run the pure metric engine. An
            // `error` severity here is a diagnosis, not a refusal.
            let device = crate::device::collect();
            let target = match pick_monitor(&device, monitor.as_deref()) {
                Ok(target) => target.clone(),
                Err(message) => {
                    pending.responses.push(Response::err(
                        id,
                        cascade_core::ipc::ErrorCode::NotFound,
                        message,
                    ));
                    return;
                }
            };
            let summary = device_summary(&device);
            let report = match path {
                None => AssessReport {
                    monitor: target.id.clone(),
                    device: summary,
                    recommendation: recommend(&target),
                    video: None,
                    warnings: Vec::new(),
                },
                Some(path) => {
                    if !Path::new(&path).is_file() {
                        pending.responses.push(Response::err(
                            id,
                            cascade_core::ipc::ErrorCode::FileMissing,
                            format!("video file not found: {path}"),
                        ));
                        return;
                    }
                    let (meta, refreshed) = match descriptor_for(ctx, &path) {
                        Ok(pair) => pair,
                        Err(reason) => {
                            pending.responses.push(Response::err(
                                id,
                                cascade_core::ipc::ErrorCode::UnsupportedCodec,
                                format!("cannot assess {path}: {reason}"),
                            ));
                            return;
                        }
                    };
                    if refreshed {
                        persist_library(ctx);
                    }
                    let Assessment {
                        warnings,
                        recommendation,
                    } = assess(&meta, &device, &target, Fit::Contain);
                    AssessReport {
                        monitor: target.id.clone(),
                        device: summary,
                        recommendation,
                        video: Some(meta),
                        warnings,
                    }
                }
            };
            match serde_json::to_value(report) {
                Ok(value) => pending.responses.push(Response::ok(id, value)),
                Err(err) => pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::Internal,
                    format!("could not build the assess report: {err}"),
                )),
            }
        }
    }
}

/// The D13 staleness proof for one file: modification time in Unix
/// seconds and byte size. Either side can fail to read (unreadable
/// metadata, pre-epoch mtime) — `None` on that side means "cannot prove
/// freshness", and [`LibraryItem::proof_matches`] then treats the entry
/// as stale instead of guessing.
fn file_proof(path: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs());
    (mtime, Some(meta.len()))
}

/// D13: the descriptor for a path — reuse the library's stored metadata
/// while its `mtime`+`size` proof still matches the file on disk, else
/// probe the container (the SourceReader dies inside that call; plain
/// data comes back) and refresh the stored entry so the new proof and
/// metadata persist across the next restart.
///
/// Returns the descriptor plus whether an entry was refreshed — the
/// caller persists the library only then. Never a load, never a retained
/// MF object (D13 leak rules: the 2026-11-08 ~50 MB/load leak stays
/// closed, design-m4 §8.3).
fn descriptor_for(ctx: &mut Loop, path: &str) -> Result<(VideoMetadata, bool), String> {
    let (mtime, size) = file_proof(Path::new(path));
    if let Some(entry) = ctx.library.find_by_path(path) {
        if entry.proof_matches(mtime, size) {
            return Ok((entry.metadata(), false));
        }
    }
    let meta = (ctx.probe)(Path::new(path))?;
    let key = cascade_core::library::dedupe_key(path);
    let mut refreshed = false;
    if let Some(entry) = ctx
        .library
        .items
        .iter_mut()
        .find(|item| cascade_core::library::dedupe_key(&item.path) == key)
    {
        let mut fresh = LibraryItem::new(
            entry.id.clone(),
            entry.path.clone(),
            meta.clone(),
            entry.added_at,
        );
        if let (Some(mtime), Some(size)) = (mtime, size) {
            fresh = fresh.with_proof(mtime, size);
        }
        *entry = fresh;
        refreshed = true;
    }
    Ok((meta, refreshed))
}

/// The device facts an `assess` response carries (`ipc.md` §3).
fn device_summary(device: &DeviceProfile) -> DeviceSummary {
    DeviceSummary {
        gpu_decode: device
            .decode
            .as_ref()
            .map(|decode| decode.gpu_codecs.clone())
            .unwrap_or_default(),
        ram_total_mb: device.ram.as_ref().map(|ram| ram.total_mb),
        monitors: device
            .monitors
            .iter()
            .map(|monitor| MonitorSummary {
                id: monitor.id.clone(),
                width: monitor.rect.w,
                height: monitor.rect.h,
                refresh_hz: monitor.refresh_hz,
                hdr: monitor.hdr,
                primary: monitor.primary,
            })
            .collect(),
    }
}

/// The monitor an assessment targets: the requested id, else the primary,
/// else the first row. A named monitor that does not exist gets the same
/// `not_found` wording `set` uses (design-m4 §5.4: assess per monitor).
fn pick_monitor<'a>(
    device: &'a DeviceProfile,
    requested: Option<&str>,
) -> Result<&'a MonitorProfile, String> {
    if let Some(id) = requested {
        return device
            .monitors
            .iter()
            .find(|monitor| monitor.id == id)
            .ok_or_else(|| format!("no monitor {id}; run `cascade status` to list them"));
    }
    device
        .monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or_else(|| device.monitors.first())
        .ok_or_else(|| "no display is attached to this session".to_string())
}

/// `library.add` (design-m4 §4): canonicalise each path, check it is a
/// file, probe its metadata, dedupe by canonical path. Rejections are
/// per-path — one bad file never fails the batch, and nothing is ever
/// copied or modified (AGENTS §3.3).
///
/// D13 probe-once: an already-stored entry whose `mtime`+`size` proof
/// still matches is reused without touching Media Foundation at all; a
/// replaced file at the same path is re-probed here. Every accepted item
/// also gets its assessment (step 5 of design-m4 §5.3) so the M5 badge
/// data exists from the moment the item enters the library — errors in
/// it never block an add (the library is paths only).
fn library_add(ctx: &mut Loop, paths: Vec<String>) -> LibraryAddResult {
    let mut result = LibraryAddResult {
        added: Vec::new(),
        rejected: Vec::new(),
        assessments: Vec::new(),
    };
    let seed = clock_seed();
    for (index, raw) in paths.into_iter().enumerate() {
        // Canonicalise for storage and dedupe; a failure here almost
        // always means the file does not exist (or is unreachable).
        let canonical = match std::fs::canonicalize(&raw) {
            Ok(path) => path,
            Err(err) => {
                result.rejected.push(RejectedPath {
                    path: raw,
                    reason: format!("file not found: {err}"),
                });
                continue;
            }
        };
        if !canonical.is_file() {
            result.rejected.push(RejectedPath {
                path: raw,
                reason: "not a file".to_string(),
            });
            continue;
        }
        let display = cascade_core::library::display_path(&canonical.to_string_lossy());
        let (mtime, size) = file_proof(&canonical);
        let cached = ctx
            .library
            .find_by_path(&display)
            .filter(|entry| entry.proof_matches(mtime, size))
            .map(LibraryItem::metadata);
        let meta = match cached {
            Some(meta) => meta,
            None => match (ctx.probe)(Path::new(&canonical)) {
                Ok(meta) => meta,
                Err(reason) => {
                    result.rejected.push(RejectedPath {
                        path: display,
                        reason,
                    });
                    continue;
                }
            },
        };
        let id = ctx.library.generate_id(seed.wrapping_add(index as u64));
        let mut item = LibraryItem::new(id, display, meta, clock_seed());
        if let (Some(mtime), Some(size)) = (mtime, size) {
            item = item.with_proof(mtime, size);
        }
        // `Added` and `Updated` (a re-add refreshes metadata) both land in
        // `added` — the existing entry is what the caller gets back.
        let item = match ctx.library.add(item) {
            cascade_core::library::AddOutcome::Added(item)
            | cascade_core::library::AddOutcome::Updated(item) => item,
        };
        result.added.push(item);
    }
    // One device collection per batch, only when something got in: the
    // primary monitor is the badge's default target (the GUI passes the
    // assigned one later, design-m4 §5.4).
    if !result.added.is_empty() {
        let device = crate::device::collect();
        if let Ok(target) = pick_monitor(&device, None) {
            let target = target.clone();
            let entries: Vec<(String, String, VideoMetadata)> = result
                .added
                .iter()
                .map(|item| (item.id.clone(), item.path.clone(), item.metadata()))
                .collect();
            for (id, path, meta) in entries {
                let assessment = assess(&meta, &device, &target, Fit::Contain);
                result.assessments.push(ItemAssessment {
                    id,
                    path,
                    warnings: assessment.warnings,
                    recommendation: assessment.recommendation,
                });
            }
        }
    }
    result
}

/// Persist after any successful mutation; a read-only profile dir costs
/// durability, never the running daemon (same rule as `config::save`).
fn persist_library(ctx: &Loop) {
    if let Err(err) = ctx.lib_store.save(ctx.library) {
        derr!("library: {err}");
    }
}

/// Seed for id generation and `added_at`: wall-clock nanoseconds folded
/// with the process id so two daemons (in different sessions) do not
/// march in lockstep.
fn clock_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id()) << 32)
}

/// `wallpaper.set` (design-m4 S2). Order matters: validate the monitor,
/// check the file, and hand the client area back to the colour presenter
/// *before* any playback surgery — every failure path then leaves the
/// desktop painted (AGENTS §3.1).
///
/// Is the backend already bound to exactly the window this set would
/// attach to? A `None` target (no host window) never qualifies.
fn reuse_surface(loaded: bool, attached: Option<isize>, target: Option<isize>) -> bool {
    loaded && target.is_some() && attached == target
}

/// What one `wallpaper.set` does in this process (design-m2 §4):
///
/// | Action   | When | Cost |
/// |----------|------|------|
/// | `Skip`   | same file already playing on the very window | none — no load, no restart, PID unchanged |
/// | `Restart`| any other set after this process has loaded once | exit ladder + respawn; each load pipeline retains ~50 MB until process exit (design-m4 §8.3), so only a fresh process stays flat |
/// | `Load`   | first load of a fresh process (or before anything loaded) | one pipeline, the baseline cost |
///
/// `Restart` wins over `Load` once `session_loads ≥ 1` regardless of path:
/// a failed or cleared earlier load still left retention behind, and an
/// inconsistent "sometimes restart" rule would confuse users.
#[derive(Debug, PartialEq, Eq)]
enum SetAction {
    Skip,
    Restart,
    Load,
}

fn set_action(same_path: bool, reusable: bool, session_loads: u32) -> SetAction {
    if same_path && reusable {
        SetAction::Skip
    } else if session_loads >= 1 {
        SetAction::Restart
    } else {
        SetAction::Load
    }
}

/// What a `settings set` that touched `engine_mode` must do next
/// (`design-engine-mode.md` §3, AGENTS §3.15: never rebuild an engine
/// inside a live daemon — a load pipeline retains ≈50 MB until exit,
/// `design-m4.md` §8.3).
///
/// | `Keep`       | value unchanged | nothing happened: no restart, no log |
/// | `PersistOnly`| changed, no engine loaded | plain state; the next load builds with it |
/// | `Restart`    | changed while an engine holds the old route | answer, exit ladder, respawn — the child builds the engine once |
#[derive(Debug, PartialEq, Eq)]
enum PresentationModeAction {
    Keep,
    PersistOnly,
    Restart,
}

fn engine_mode_action(
    previous: PresentationMode,
    current: PresentationMode,
    backend_loaded: bool,
) -> PresentationModeAction {
    if previous == current {
        PresentationModeAction::Keep
    } else if backend_loaded {
        PresentationModeAction::Restart
    } else {
        PresentationModeAction::PersistOnly
    }
}

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
    // 2b. S5 (design-m4 §5.3): probe — staleness-aware via the library's
    //     `mtime`+`size` proof (D13), data only, never a load — and assess
    //     BEFORE the response is built: a different file restarts this
    //     process right after the client has the answer, so the warnings
    //     must already be in it. An `error` (codec policy) rejects here;
    //     `warn`/`info` print honestly and the set still applies.
    let mut report = SetReport {
        video: None,
        warnings: Vec::new(),
        recommendation: None,
    };
    let device = crate::device::collect();
    if let Ok(target) = pick_monitor(&device, monitor.as_deref()) {
        let target = target.clone();
        match descriptor_for(ctx, &path) {
            Ok((meta, refreshed)) => {
                if refreshed {
                    persist_library(ctx);
                }
                let assessment = assess(&meta, &device, &target, Fit::Contain);
                if let Some(error) = assessment.first_error() {
                    pending.responses.push(Response::err(
                        id,
                        cascade_core::ipc::ErrorCode::UnsupportedCodec,
                        format!("{} — {}", error.detail, error.fix),
                    ));
                    return;
                }
                report.video = Some(meta);
                report.warnings = assessment.warnings;
                report.recommendation = Some(assessment.recommendation);
            }
            Err(reason) => {
                pending.responses.push(Response::err(
                    id,
                    cascade_core::ipc::ErrorCode::UnsupportedCodec,
                    format!("cannot play {path}: {reason}"),
                ));
                return;
            }
        }
    } else {
        // No monitor row to assess against — apply the video without
        // claims rather than refuse it (never invent a measurement).
        dlog!("set: no display profile; skipping the assessment");
    }
    // 3. Skip / restart / load decision (design-m2 §4). The same file
    //    already playing on the very window: a pure no-op — no load, no
    //    restart, no growth. Anything else once this process has loaded:
    //    restart it (the client is answered first; the flush ack then runs
    //    the exit ladder and respawns). Only a fresh process loads here.
    let host_windows = ctx.host.windows();
    let target_hwnd = monitor
        .as_deref()
        .and_then(|id| host_windows.iter().find(|window| window.monitor.id == id))
        .or_else(|| host_windows.first())
        .map(|window| window.hwnd as isize);
    let same_path = ctx.state.video.as_deref() == Some(path.as_str());
    let reusable = reuse_surface(ctx.backend.loaded, ctx.backend.attached_hwnd, target_hwnd);
    match set_action(same_path, reusable, ctx.backend.session_loads) {
        SetAction::Skip => {
            dlog!("set: already playing this file on this window; no-op");
            // The same window can be named differently (None vs. its id);
            // keep state.json in step with the request either way.
            if ctx.state.monitor != monitor {
                ctx.state.monitor = monitor;
                if let Err(err) = ctx.store.save(ctx.state) {
                    derr!("state: {err}");
                }
            }
            push_set_ok(pending, id, report);
            return;
        }
        SetAction::Restart => {
            // Persist the new path first: the child process reads it back
            // on start (design-m2 §4). The client receives this answer, the
            // flush ack runs the exit ladder (repaint included), then the
            // process respawns and the child loads the file with flat RAM.
            ctx.state.video = Some(path.clone());
            ctx.state.monitor = monitor;
            if let Err(err) = ctx.store.save(ctx.state) {
                derr!("state: {err}");
            }
            let value = match serde_json::to_value(&report) {
                Ok(value) => value,
                Err(err) => {
                    derr!("set: could not build the set report: {err}");
                    no_data()
                }
            };
            let mut response = Response::ok(id, value);
            response.flush_after = true;
            pending.responses.push(response);
            pending.respawn = Some(id);
            dlog!("set: different video; restarting the daemon to release retained memory");
            return;
        }
        SetAction::Load => dlog!("set: loading {}", path),
    }
    // 4. Detach the engine while its playback HWND is still alive, then
    //    take the client area back for the colour presenter (design-m2 §4).
    if let Err(err) = ctx.backend.detach_surface() {
        derr!("set: backend detach failed: {err}");
    }
    ctx.host.restore_presenter();
    // 5. Build a backend on demand: `cascade-win` may not depend on
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
    // 6. Load the new source. A failure keeps the colour (step 4) and
    //    drops the broken path so the next start does not replay it.
    let failure = match ctx.backend.backend.as_mut() {
        // The requested engine route rides the existing boundary
        // (`design-engine-mode.md` §4): read here, once, per load.
        Some(backend) => backend
            .load(
                Path::new(&path),
                &PlaybackOptions {
                    engine_mode: ctx.settings.engine_mode,
                    ..PlaybackOptions::default()
                },
            )
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
    // One more pipeline retained ~50 MB (design-m4 §8.3): from now on this
    // process restarts itself instead of loading again (design-m2 §4).
    ctx.backend.session_loads += 1;
    // 7. Persist so the next start replays this file (design-m2 §5).
    ctx.state.video = Some(path.clone());
    ctx.state.monitor = monitor;
    if let Err(err) = ctx.store.save(ctx.state) {
        derr!("state: {err}");
    }
    // 8. Attach to the assigned monitor, then apply the pause rules.
    attach_backend(ctx.backend, ctx.host, ctx.state.monitor.as_deref());
    apply_playback(
        ctx.backend,
        ctx.settings,
        ctx.flags,
        control::session_locked(),
    );
    push_set_ok(pending, id, report);
}

/// The `wallpaper.set` success payload (S5): the assessment that ran
/// before the answer. A serialization failure of plain data is logged
/// and answered with an empty ok — the video is applied by then and an
/// internal error must not claim otherwise.
fn push_set_ok(pending: &mut Pending, id: u64, report: SetReport) {
    match serde_json::to_value(&report) {
        Ok(value) => pending.responses.push(Response::ok(id, value)),
        Err(err) => {
            derr!("set: could not build the set report: {err}");
            pending.responses.push(Response::ok(id, no_data()));
        }
    }
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

/// How `run()` ended (design-m2 §4). `Exit` is any normal stop; `Respawn`
/// means a different-file `wallpaper.set` was answered, the exit ladder
/// has run, and the caller must re-exec this binary so the child loads
/// the new file with flat memory.
#[derive(Debug, PartialEq, Eq)]
pub enum RunExit {
    Exit,
    Respawn,
}

/// How long a respawn child waits for its parent to release the session
/// pipe (design-m2 §4): the parent only runs its exit ladder and returns
/// from `main`, so seconds are ample — and a hang here still degrades to
/// the normal single-instance error, never to a second daemon.
const RESPAWN_PIPE_WAIT: Duration = Duration::from_secs(5);
const RESPAWN_PIPE_POLL: Duration = Duration::from_millis(100);

/// Create the session pipe on a `CASCADE_RESPAWN` start: the parent owns
/// it until the process exits, so retry the contention instead of reading
/// it as another live daemon. After the deadline the normal single-
/// instance verdict applies (`control` may already be gone by then, in
/// which case the raw pipe error is returned).
fn create_respawn_pipe() -> Result<PipeServer, DaemonError> {
    let deadline = Instant::now() + RESPAWN_PIPE_WAIT;
    loop {
        match PipeServer::create() {
            Ok(pipe) => return Ok(pipe),
            Err(code) => {
                if Instant::now() >= deadline {
                    if let Some(existing) = control::find_existing() {
                        return Err(DaemonError::AlreadyRunning(existing as usize));
                    }
                    return Err(DaemonError::Pipe(code));
                }
                std::thread::sleep(RESPAWN_PIPE_POLL);
            }
        }
    }
}

/// Run the daemon until asked to stop. Always repaints the desktop before
/// returning, including on the restore path.
pub fn run(
    opts: DaemonOptions,
    backend: Option<Box<dyn MediaBackend>>,
    factory: BackendFactory,
    probe: ProbeFn,
) -> Result<RunExit, DaemonError> {
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
        attached_hwnd: None,
        session_loads: 0,
    };

    // The pipe is the primary single-instance lock (`ipc.md` §1): the first
    // daemon creates it with `FILE_FLAG_FIRST_PIPE_INSTANCE`, a second one
    // fails here. The control window check stays as the fallback for an
    // older daemon that has no pipe yet. On any failure `pipe` drops and
    // closes the handle, so nothing is left behind.
    //
    // A respawn child (design-m2 §4) spawns while its parent still owns
    // the pipe, so contention is expected, not a second daemon: retry
    // until the parent's exit ladder finishes instead of failing fast.
    let pipe = if std::env::var_os("CASCADE_RESPAWN").is_some() {
        create_respawn_pipe()?
    } else {
        match PipeServer::create() {
            Ok(pipe) => pipe,
            Err(code) => {
                if let Some(existing) = control::find_existing() {
                    return Err(DaemonError::AlreadyRunning(existing as usize));
                }
                return Err(DaemonError::Pipe(code));
            }
        }
    };
    if let Some(existing) = control::find_existing() {
        return Err(DaemonError::AlreadyRunning(existing as usize));
    }

    install_panic_hook();

    let store = Store::open();
    let previous = store.load();
    // The library (design-m4 §4): loaded once, pruned on start so a file
    // deleted while the daemon was down disappears from the list.
    let lib_store = LibraryStore::open();
    let mut library = lib_store.load();
    let pruned = library.prune(|path| Path::new(path).is_file());
    if pruned > 0 {
        dlog!("library: pruned {pruned} entries with missing files at start");
        if let Err(err) = lib_store.save(&library) {
            derr!("library: {err}");
        }
    }
    if let Some(path) = lib_store.path() {
        dlog!(
            "  library: {} items, {}",
            library.items.len(),
            path.display()
        );
    }
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
            // The startup load carries the persisted route too: the child
            // of a mode-toggle respawn builds the engine once, in it
            // (`design-engine-mode.md` §3).
            Some(backend) => match backend.load(
                source,
                &PlaybackOptions {
                    engine_mode: settings.engine_mode,
                    ..PlaybackOptions::default()
                },
            ) {
                Ok(()) => {
                    dlog!("  video: {}", source.display());
                    backend_guard.loaded = true;
                    // The startup pipeline counts: any later set must
                    // restart instead of loading again (design-m2 §4).
                    backend_guard.session_loads += 1;
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
    let first = Arc::new(current_snapshot(&StatusInputs {
        host: &host,
        monitors: &monitors,
        opts: &opts,
        settings: &settings,
        flags: &flags,
        backend: &backend_guard,
        state: &state,
        library: &library,
    }));
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
        flush_acked: flush_tx,
    }) {
        // The desktop must stay up regardless: log and continue without a
        // control channel (the control-window fallback still detects a
        // second daemon).
        derr!("ipc: could not start the pipe thread (error {code}); no `cascade status`");
    }

    let mut msg = MSG::default();
    // Set when a `wallpaper.set` decided this process must re-exec; read
    // once after the exit ladder to pick the `run()` result (design-m2 §4).
    let mut respawn_after = false;
    // S2b power collector (design-m4 §2.6): registration handle, the pure
    // hysteresis state, the D8 gate, and a pending reload-respawn. All four
    // are inert while `settings.on_battery == off` — no read, no subscribe.
    let mut power_registration: Option<isize> = None;
    let mut power_hysteresis = power::Hysteresis::default();
    let mut collector_on = false;
    let mut power_respawn = false;
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
                library: &mut library,
                lib_store: &lib_store,
                probe: &probe,
            };
            let mut pending = Pending::default();
            while let Ok(command) = cmd_rx.try_recv() {
                handle_command(&mut ctx, command, &mut pending);
            }
            pending
        };
        let Pending {
            responses,
            quit,
            respawn,
        } = pending;
        if !responses.is_empty() {
            refresh_status(
                &msg_tx,
                &mut published,
                &StatusInputs {
                    host: &host,
                    monitors: &monitors,
                    opts: &opts,
                    settings: &settings,
                    flags: &flags,
                    backend: &backend_guard,
                    state: &state,
                    library: &library,
                },
            );
            for response in responses {
                if msg_tx.send(ServerMsg::Response(response)).is_err() {
                    derr!("ipc: response channel closed; the client will time out");
                }
            }
        }
        if quit || respawn.is_some() || power_respawn {
            // The pipe thread flushes its write to the client, then acks;
            // only then does the exit ladder start (design-m4 §2.3 — a
            // respawn-triggering `wallpaper.set` shares that handshake,
            // design-m2 §4). A missing ack must not keep the daemon alive.
            // A power respawn (S2b) has no client waiting, so it must not
            // sit through the flush timeout either.
            if quit || respawn.is_some() {
                if let Err(err) = flush_rx.recv_timeout(QUIT_FLUSH_WAIT) {
                    derr!("ipc: flush ack missing ({err}); quitting anyway");
                }
            }
            if respawn.is_some() || (power_respawn && !quit) {
                respawn_after = true;
            }
            power_respawn = false;
            control::request_quit();
        }
        if control::quit_requested() {
            break;
        }

        // S2b: one collector turn per iteration (design-m4 §2.6) — gate,
        // raw sample through the 3 s hysteresis, reconcile. Placed before
        // the wait so a broadcast dispatched at the end of the previous
        // iteration is consumed immediately and a `settings set` that just
        // changed `on_battery` opens or closes the gate without an extra
        // watchdog delay. It never blocks and never reads hardware unless
        // the gate is open.
        if power_step(PowerStep {
            hwnd: control.hwnd(),
            settings: &settings,
            flags: &mut flags,
            backend: &mut backend_guard,
            host: &mut host,
            state: &state,
            registration: &mut power_registration,
            hysteresis: &mut power_hysteresis,
            collector_on: &mut collector_on,
        }) {
            // The video must come back but this process already loaded
            // once: head straight for the quit block at the top — no
            // wait, the child does the reload with flat memory (§8.3).
            power_respawn = true;
            continue;
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
                &StatusInputs {
                    host: &host,
                    monitors: &monitors,
                    opts: &opts,
                    settings: &settings,
                    flags: &flags,
                    backend: &backend_guard,
                    state: &state,
                    library: &library,
                },
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
            &StatusInputs {
                host: &host,
                monitors: &monitors,
                opts: &opts,
                settings: &settings,
                flags: &flags,
                backend: &backend_guard,
                state: &state,
                library: &library,
            },
        );
    }

    // Critical work first, prints after: a console close (CTRL_CLOSE) can
    // kill this process as soon as the control handler returns, and that
    // handler waits for `ExitSignal` — which drops at the end of this
    // function. The desktop must be repainted and the state saved before
    // anything else, including logging.
    dlog!("stopping");
    // S2b: drop the power subscription while the control window it points
    // at is still alive (design-m4 §2.6). The OS would reclaim it on
    // process exit anyway; doing it here keeps the respawn path symmetric.
    if let Some(handle) = power_registration.take() {
        power::unregister(handle);
    }
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
    if respawn_after {
        Ok(RunExit::Respawn)
    } else {
        Ok(RunExit::Exit)
    }
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
            attached_hwnd: None,
            session_loads: 0,
        };
        let settings = Settings::default();
        let mut flags = Flags {
            user_paused: true,
            ..Flags::default()
        };

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
            attached_hwnd: None,
            session_loads: 0,
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
            attached_hwnd: None,
            session_loads: 0,
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
            attached_hwnd: None,
            session_loads: 0,
        };
        apply_playback(
            &mut no_backend,
            &Settings::default(),
            &Flags::default(),
            true,
        );
    }

    #[test]
    fn reuse_surface_only_for_a_loaded_engine_on_the_same_window() {
        // Loaded and bound to exactly the target window: reuse.
        assert!(reuse_surface(true, Some(0x1234), Some(0x1234)));
        // No video loaded (cleared / first set): full path.
        assert!(!reuse_surface(false, None, Some(0x1234)));
        // Attached elsewhere (monitor switch): full path recreates.
        assert!(!reuse_surface(true, Some(0x1234), Some(0x5678)));
        // Detached (clear / restore / repair in flight): full path.
        assert!(!reuse_surface(true, None, Some(0x1234)));
        // No host window at all: never reuse.
        assert!(!reuse_surface(true, Some(0x1234), None));
        assert!(!reuse_surface(false, None, None));
    }

    #[test]
    fn set_action_covers_the_eight_decision_cells() {
        // Same file, already on this window: skip — regardless of loads.
        assert_eq!(set_action(true, true, 0), SetAction::Skip);
        assert_eq!(set_action(true, true, 1), SetAction::Skip);
        assert_eq!(set_action(true, true, 7), SetAction::Skip);
        // Same file but not reusable (other window / detached): restart
        // once this process has loaded, else the first load runs.
        assert_eq!(set_action(true, false, 1), SetAction::Restart);
        assert_eq!(set_action(true, false, 0), SetAction::Load);
        // Different file: restart after any load, load on a fresh process.
        assert_eq!(set_action(false, true, 1), SetAction::Restart);
        assert_eq!(set_action(false, true, 0), SetAction::Load);
        assert_eq!(set_action(false, false, 1), SetAction::Restart);
        assert_eq!(set_action(false, false, 0), SetAction::Load);
    }

    #[test]
    fn engine_mode_change_decides_between_keep_persist_and_restart() {
        // Same value: a no-op, whatever the engine is doing.
        assert_eq!(
            engine_mode_action(PresentationMode::Accurate, PresentationMode::Accurate, true),
            PresentationModeAction::Keep
        );
        assert_eq!(
            engine_mode_action(
                PresentationMode::Efficient,
                PresentationMode::Efficient,
                false
            ),
            PresentationModeAction::Keep
        );
        // Changed with an engine holding the old route: respawn (AGENTS
        // §3.15 — never rebuild in-process).
        assert_eq!(
            engine_mode_action(
                PresentationMode::Accurate,
                PresentationMode::Efficient,
                true
            ),
            PresentationModeAction::Restart
        );
        assert_eq!(
            engine_mode_action(
                PresentationMode::Efficient,
                PresentationMode::Accurate,
                true
            ),
            PresentationModeAction::Restart
        );
        // Changed with nothing loaded: persist only, no process churn.
        assert_eq!(
            engine_mode_action(
                PresentationMode::Accurate,
                PresentationMode::Efficient,
                false
            ),
            PresentationModeAction::PersistOnly
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
            "D8 default: no collector ran, so the confirmed source is off"
        );

        flags.user_paused = true;
        flags.on_battery = true;
        let state = session_state(&settings, &flags, true);
        assert!(state.user_paused, "runtime flag wins over defaults");
        assert!(state.session_locked);
        assert!(
            state.on_battery,
            "the collector's confirmed source reaches the oracle (S2b)"
        );
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
