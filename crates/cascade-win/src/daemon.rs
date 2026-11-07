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
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use cascade_core::rule_engine::{PauseSettings, RuleEngine, SessionState};
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

use crate::control::{self, ControlWindow};
use crate::host::{self, Host, HostOptions};
use crate::monitor::{self, MonitorInfo};
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

#[derive(Debug)]
pub enum DaemonError {
    AlreadyRunning(usize),
    Control(u32),
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

/// One watchdog tick. Returns `true` when the desktop is healthy afterwards.
fn watchdog_tick(
    host: &mut Host,
    surface: &mut ShellSurface,
    monitors: &[MonitorInfo],
    opts: &DaemonOptions,
    snapshot: &WallpaperSnapshot,
    backend: &mut BackendGuard,
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
            attach_backend(backend, host);
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

/// Attach the backend to the first host window (design-m2 §5: one engine,
/// the other monitors keep colour). Failures are logged, never fatal — the
/// desktop must stay up even when the engine cannot draw.
fn attach_backend(backend: &mut BackendGuard, host: &mut Host) {
    if !backend.loaded {
        return;
    }
    let Some(surface) = host.windows().first().map(host_surface) else {
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

/// Apply one session lock/unlock transition through the rule engine
/// (M3 slice, design-m3 §3). Pausing freezes the last frame — the desktop
/// is never cleared (AGENTS §3.1).
fn apply_session_lock(backend: &mut BackendGuard, locked: bool) {
    if !backend.loaded {
        return;
    }
    let state = SessionState {
        session_locked: locked,
        pause_settings: PauseSettings {
            session_lock: true,
            ..PauseSettings::default()
        },
        ..SessionState::default()
    };
    let desired = RuleEngine::evaluate(state);
    let Some(engine) = backend.backend.as_mut() else {
        return;
    };
    if desired.playing {
        dlog!("session unlocked; resuming playback");
        if let Err(err) = engine.play() {
            derr!("resume: {err}");
        }
    } else {
        dlog!(
            "session locked; pausing playback ({})",
            desired.pause_reason.map_or("?", |reason| reason.as_str())
        );
        if let Err(err) = engine.pause() {
            derr!("pause: {err}");
        }
    }
}

/// Run the daemon until asked to stop. Always repaints the desktop before
/// returning, including on the restore path.
pub fn run(opts: DaemonOptions, backend: Option<Box<dyn MediaBackend>>) -> Result<(), DaemonError> {
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

    if let Some(existing) = control::find_existing() {
        return Err(DaemonError::AlreadyRunning(existing as usize));
    }

    install_panic_hook();

    let store = Store::open();
    let previous = store.load();

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
    attach_backend(&mut backend_guard, &mut host);

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
    );
    if let Err(err) = store.save(&state) {
        derr!("state: {err}");
    }
    if let Some(path) = store.path() {
        dlog!("  state: {}", path.display());
    }
    dlog!("  Ctrl+C in this console, or `cascade stop`, restores the wallpaper and quits");

    let mut msg = MSG::default();
    while !control::quit_requested() {
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
            dlog!("restore requested");
            if let Err(err) = backend_guard.detach_surface() {
                derr!("restore: backend detach failed: {err}");
            }
            host.detach();
            let report = repaint::ladder(&surface, &snapshot, opts.repaint_level);
            print_repaint("restore", &report);
            match reattach(&monitors, &opts) {
                Some((new_surface, new_host)) => {
                    surface = new_surface;
                    host = new_host;
                    print_attach(&host);
                    attach_backend(&mut backend_guard, &mut host);
                }
                None => {
                    derr!("restore: Explorer did not return; the watchdog will keep trying");
                }
            }
        }

        if let Some(locked) = control::take_session_lock() {
            apply_session_lock(&mut backend_guard, locked);
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
                        attach_backend(&mut backend_guard, &mut host);
                    }
                    None => {
                        derr!("display change: reattach failed; the watchdog will keep trying");
                    }
                }
            }
        }
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
    fn session_lock_pauses_and_unlock_resumes() {
        let (mock, pauses, plays) = mock_backend();
        let mut backend = BackendGuard {
            backend: Some(Box::new(mock)),
            loaded: true,
        };

        apply_session_lock(&mut backend, true);
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
        assert_eq!(plays.load(Ordering::SeqCst), 0);

        apply_session_lock(&mut backend, false);
        assert_eq!(plays.load(Ordering::SeqCst), 1);
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session_lock_without_video_is_a_no_op() {
        // No backend box at all: must not panic.
        let mut no_backend = BackendGuard {
            backend: None,
            loaded: true,
        };
        apply_session_lock(&mut no_backend, true);

        // Backend present but no video loaded: rule must not reach it.
        let (mock, pauses, plays) = mock_backend();
        let mut not_loaded = BackendGuard {
            backend: Some(Box::new(mock)),
            loaded: false,
        };
        apply_session_lock(&mut not_loaded, true);
        assert_eq!(pauses.load(Ordering::SeqCst), 0);
        assert_eq!(plays.load(Ordering::SeqCst), 0);
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
