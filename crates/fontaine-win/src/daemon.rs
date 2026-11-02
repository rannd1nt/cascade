//! The daemon: own the control window, attach the host, run the message loop.
//!
//! The loop waits with `MsgWaitForMultipleObjectsEx` and wakes every two
//! seconds instead of blocking in `GetMessageW`. That tick is the watchdog:
//! an Explorer restart recreates `Progman` and every `WorkerW`, a resolution
//! change moves the monitors, and another wallpaper app can push its window
//! above ours. All of those show up as "the host is no longer healthy" and are
//! repaired in place, so the user never has to restart Fontaine after a crash,
//! a logon, or an Explorer restart.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{COLORREF, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostMessageW, TranslateMessage,
    MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_CLOSE, WM_QUIT,
};

use crate::console;
use crate::control::{self, ControlWindow};
use crate::host::{self, Host, HostOptions};
use crate::monitor::{self, MonitorInfo};
use crate::repaint;
use crate::shell::{self, ShellError, ShellSurface};
use crate::state::{DaemonState, Store};
use crate::wallpaper::WallpaperSnapshot;

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
                "Fontaine is already running (control window 0x{hwnd:x}); use `fontaine restore` or `fontaine stop`"
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

unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> BOOL {
    // Posting keeps the handler itself short: Windows waits for it to return.
    let hwnd = CONTROL_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() {
        // SAFETY: the control window belongs to this process and outlives the
        // handler; PostMessageW only enqueues.
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    }
    1
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("fontaine: panic: {info}");
        // Best effort: never panic inside the panic hook.
        host::emergency_cleanup(2);
    }));
}

fn print_attach(host: &Host) {
    println!(
        "attached: mode={} progman=0x{:x} workerw=0x{:x} defview=0x{:x}, {} host window(s)",
        host.surface().mode.label(),
        host.surface().progman as usize,
        host.surface().workerw as usize,
        host.surface().defview as usize,
        host.windows().len()
    );
    for line in host.describe() {
        println!("  {line}");
    }
}

fn print_repaint(label: &str, report: &repaint::RepaintReport) {
    for attempt in &report.attempts {
        println!(
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
                Err(err) => eprintln!("reattach: {err}"),
            },
            Err(err) => eprintln!("reattach: {err}"),
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
    eprintln!("watchdog: host window is no longer healthy; repairing");

    // Cheap repair first: another window may simply have been raised above us.
    if shell::surface_is_alive(surface) {
        host.restack();
        if host.is_healthy() {
            println!("watchdog: z-order restored");
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
            eprintln!("watchdog: Explorer is not back yet; retrying on the next tick");
            false
        }
    }
}

/// Run the daemon until asked to stop. Always repaints the desktop before
/// returning, including on the restore path.
pub fn run(opts: DaemonOptions) -> Result<(), DaemonError> {
    if let Some(existing) = control::find_existing() {
        return Err(DaemonError::AlreadyRunning(existing as usize));
    }

    // Started by the Run key or by double-click: close the console window we
    // own so the daemon stays silent. Started from a terminal: keep it.
    let console_detached = console::detach_if_private();

    install_panic_hook();

    let store = Store::open();
    let previous = store.load();

    let control = ControlWindow::create().map_err(DaemonError::Control)?;
    CONTROL_HWND.store(control.hwnd() as isize, Ordering::Release);
    // SAFETY: handler stays valid for the process lifetime; failure only means
    // there is no console attached (the autostart case).
    unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) };

    let snapshot = crate::wallpaper::snapshot();
    println!(
        "fontaine {} starting{}",
        env!("CARGO_PKG_VERSION"),
        if console_detached {
            " (console detached)"
        } else {
            ""
        }
    );
    // The executable path makes bug reports unambiguous: it shows exactly
    // which build is running and from where (install dir vs. a temp copy).
    if let Ok(exe) = std::env::current_exe() {
        println!("  exe: {}", exe.display());
    }
    println!(
        "  wallpaper snapshot: {}",
        snapshot.path.as_deref().unwrap_or("(none)")
    );
    if let Some(previous) = &previous {
        println!(
            "  previous state: running={} pid={} updated_unix={}",
            previous.running, previous.pid, previous.updated_unix
        );
    }

    let mut surface = shell::discover(opts.shell_timeout).map_err(DaemonError::Shell)?;

    // A previous session that died without a clean exit can leave Explorer
    // with a stale desktop composition. Repaint once before we draw on it.
    if previous.as_ref().is_some_and(|state| state.running) {
        println!("  previous session did not stop cleanly; repainting the desktop first");
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
        eprintln!("state: {err}");
    }
    if let Some(path) = store.path() {
        println!("  state: {}", path.display());
    }
    println!("  Ctrl+C in this console, or `fontaine stop`, restores the wallpaper and quits");

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
            println!("restore requested");
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
                    eprintln!("restore: Explorer did not return; the watchdog will keep trying");
                }
            }
        }

        if control::take_display_change() {
            println!("display change; re-reading monitors");
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
                        eprintln!("display change: reattach failed; the watchdog will keep trying");
                    }
                }
            }
        }
    }

    println!("stopping");
    CONTROL_HWND.store(0, Ordering::Release);
    host.detach();
    let report = repaint::ladder(&surface, &snapshot, opts.repaint_level);
    print_repaint("exit", &report);

    state.running = false;
    if let Err(err) = store.save(&state) {
        eprintln!("state: {err}");
    }
    Ok(())
}
