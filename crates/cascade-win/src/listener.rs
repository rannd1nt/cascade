//! The battery listener (`design-m5b.md` §4): pipe + control window, no
//! engine — zero compute by construction.
//!
//! Steady state is two blocked threads: this one in `GetMessageW`, the pipe
//! thread in `ConnectNamedPipe`. No backend, no host windows, no timers, no
//! power polling (§4.2). The desktop is never touched, so every exit path
//! — including a crash — leaves the real wallpaper standing (§4.4 T11).

use std::sync::mpsc;
use std::sync::Arc;

use cascade_core::ipc::Settings;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

use crate::control::{self, ControlWindow};
use crate::daemon::{self, DaemonError, RunExit};
use crate::ipc::{PipeChannels, PipeRole, ServerMsg, StatusSnapshot};
use crate::monitor::{self, MonitorInfo};
use crate::offline;
use crate::{derr, dlog};

/// Run the listener until asked to stop (`design-m5b.md` §4.5). Returns
/// only when the control window closed (`cascade stop`, window teardown) or
/// the pipe answered a `quit` — always as [`RunExit::Exit`]: the listener
/// never respawns (MD4, §7 — verified 2026-11-19, recorded in §9).
///
/// Structural skip-load (`design-m5b.md` §4.3): the binary enters here
/// *before* `build_backend`, so no backend, factory call, or Media
/// Foundation session can exist in this process.
pub fn run() -> Result<RunExit, DaemonError> {
    // Declared first, therefore dropped last on every exit path — the same
    // guard as `daemon::run` (`design-m5b.md` §4.9).
    let _exit = daemon::ExitSignal;

    // One pipe, one control window: the same single-instance lock the full
    // daemon takes (`design-m5b.md` §4.2), so a listener and a daemon can
    // never coexist — whichever comes second gets `AlreadyRunning`.
    let pipe = daemon::create_session_pipe()?;
    daemon::install_panic_hook();
    // Kept alive for the whole run: dropping it would destroy the window
    // and take `find_existing()` down with it.
    let _control = ControlWindow::create().map_err(DaemonError::Control)?;

    // Channels mirror the daemon's shape (`design-m5b.md` §4.9): the first
    // snapshot travels before the thread starts, `commands` is never sent
    // on (the listener dispatches in place), and `flush_acked` carries the
    // `quit` handshake (§4.5).
    let (msg_tx, msg_rx) = mpsc::channel();
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (flush_tx, flush_rx) = mpsc::channel();
    if msg_tx
        .send(ServerMsg::Snapshot(Arc::new(listening_snapshot(
            monitor::enumerate(),
            offline::settings_get(),
        ))))
        .is_err()
    {
        derr!("ipc: message channel rejected the first send; status stays at boot");
    }
    if let Err(code) = pipe.serve(
        PipeChannels {
            messages: msg_rx,
            commands: cmd_tx,
            flush_acked: flush_tx,
        },
        PipeRole::Listener,
    ) {
        // Same decision as the daemon (`design-m5b.md` §4.9): keep serving
        // what we can. The control window still answers `cascade stop`.
        derr!("ipc: could not start the pipe thread (error {code}); no `cascade status`");
    }

    dlog!("listener: pipe and window up; waiting (no engine by design)");

    // Blocked until a message arrives — zero wakeups in steady state
    // (§4.2): unlike the full daemon there is no host to watchdog, so the
    // 2 s tick and `MsgWaitForMultipleObjectsEx` are not needed at all.
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid out buffer; null hwnd + 0/0 filter
        // accepts every message posted to this thread (Win32 doc).
        let got = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if got <= 0 {
            // 0 = `WM_QUIT` (control window closed / `cascade stop`),
            // -1 = error: either way the listener ends. The desktop was
            // never touched, so there is nothing to repaint.
            break;
        }
        // SAFETY: `msg` was filled by `GetMessageW` above.
        unsafe { TranslateMessage(&msg) };
        // SAFETY: `msg` was filled by `GetMessageW` above.
        unsafe { DispatchMessageW(&msg) };

        if msg.message == control::WM_APP_PIPE {
            // Wake-only from the pipe thread (`design-m5b.md` §4.5): a
            // `quit` sends the flush ack *after* the client has its
            // answer, so collecting it here means the answer was written
            // and the listener may exit. A wake without the ack (a stale
            // post) changes nothing.
            if flush_rx.try_recv().is_ok() {
                control::request_quit();
            }
        }
        if control::quit_requested() {
            break;
        }
    }

    // No respawn from the listener in either direction (§4.5, MD4): it
    // exits as it is — the desktop is already the real wallpaper.
    dlog!("listener: exiting");
    Ok(RunExit::Exit)
}

/// The listener's status snapshot (`design-m5b.md` D6): honest about what
/// this process is — `mode:"listening"`, no host, no playback, monitors by
/// identity, the real file-backed settings. Injected for tests so a unit
/// test never enumerates real monitors or reads the user's config.
fn listening_snapshot(monitors: Vec<MonitorInfo>, settings: Settings) -> StatusSnapshot {
    StatusSnapshot {
        mode: "listening".to_string(),
        paused_reasons: Vec::new(),
        monitors,
        playback: None,
        host_strategy: "none".to_string(),
        repaint_level: 0,
        settings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_snapshot_matches_d6() {
        // design-m5b D6: what the listener publishes — no engine, no host,
        // no playback, monitors listed with identity fields only.
        let snapshot = listening_snapshot(Vec::new(), Settings::default());
        assert_eq!(snapshot.mode, "listening");
        assert_eq!(snapshot.host_strategy, "none");
        assert_eq!(snapshot.repaint_level, 0);
        assert!(snapshot.playback.is_none());
        assert!(snapshot.paused_reasons.is_empty());
        assert!(snapshot.monitors.is_empty());
    }
}
