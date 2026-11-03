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
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{COLORREF, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};
use windows_sys::Win32::System::Threading::{WaitOnAddress, WakeByAddressSingle};
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

#[derive(Clone, Copy, Debug)]
pub struct DaemonOptions {
    pub color: COLORREF,
    pub repaint_level: u32,
    pub shell_timeout: Duration,
    pub host_mode: host::HostModePreference,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            color: HostOptions::default().color,
            repaint_level: 1,
            shell_timeout: Duration::from_secs(60),
            host_mode: host::HostModePreference::Auto,
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
            true
        }
        None => {
            derr!("watchdog: Explorer is not back yet; retrying on the next tick");
            false
        }
    }
}

/// Run the daemon until asked to stop. Always repaints the desktop before
/// returning, including on the restore path.
pub fn run(opts: DaemonOptions) -> Result<(), DaemonError> {
    // Declared first, therefore dropped last on every exit path (`Ok`, an
    // early `Err`, and panic unwinding): only then is `EXIT_DONE` signalled,
    // which lets a waiting console-control handler return — and Windows only
    // kills the process after that return.
    let _exit = ExitSignal;

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

    let mut state = DaemonState::running_now(
        std::process::id(),
        host::rgb_hex(opts.color),
        opts.repaint_level,
        opts.host_mode.label().to_string(),
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
            watchdog_tick(&mut host, &mut surface, &monitors, &opts, &snapshot);
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
            host.detach();
            let report = repaint::ladder(&surface, &snapshot, opts.repaint_level);
            print_repaint("restore", &report);
            match reattach(&monitors, &opts) {
                Some((new_surface, new_host)) => {
                    surface = new_surface;
                    host = new_host;
                    print_attach(&host);
                }
                None => {
                    derr!("restore: Explorer did not return; the watchdog will keep trying");
                }
            }
        }

        if control::take_display_change() {
            dlog!("display change; re-reading monitors");
            monitors = monitor::enumerate();
            if !monitors.is_empty() {
                host.detach();
                match reattach(&monitors, &opts) {
                    Some((new_surface, new_host)) => {
                        surface = new_surface;
                        host = new_host;
                        print_attach(&host);
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
    }
}
