//! The battery listener: pipe + control window + tray
//! + power subscription, no engine — zero compute by construction.
//!
//! Steady state is two blocked threads: this one in `GetMessageW`, the pipe
//! thread in `ConnectNamedPipe`. No backend, no host windows, no monitor
//! loop, no render, no host watchdog. Timed work: one 2 s source-watchdog
//! timer armed for the whole run (amendment
//! 2026-11-20, measured in — a broadcast-only listener strands when the
//! OS delivers a stale or missing power broadcast), plus the ≤3 s
//! hysteresis window, whose timer exists only while a candidate is
//! pending. The desktop is never touched, so every
//! exit path — including a crash — leaves the real wallpaper standing
//!

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cascade_core::ipc::Settings;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, SetTimer, TranslateMessage, MSG, WM_TIMER,
};

use crate::control::{self, ControlWindow};
use crate::daemon::{self, DaemonError, ProbeFn, RunExit};
use crate::ipc::{PipeChannels, PipeRole, ServerMsg, StatusSnapshot};
use crate::monitor::{self, MonitorInfo};
use crate::offline;
use crate::power::{self, Hysteresis};
use crate::respawn;
use crate::tray::{Tray, TrayAction, TrayMode};
use crate::{derr, dlog};

/// Timer id of the one-shot hysteresis window.
/// `SetTimer` with a window posts `WM_TIMER` with this id; the listener
/// never inspects *this* id — the hysteresis timer exists purely as a
/// *wake source*, and the actual `tick` runs in [`ListenerPower::turn`]
/// like every other power turn. (The watchdog's id is inspected, below.)
const HYSTERESIS_TIMER: usize = 1;

/// Timer id of the source watchdog (amendment
/// 2026-11-20, evidence ). Unlike the hysteresis timer this one IS
/// identified in the loop: a `WM_TIMER` with this id brings a fallback
/// `power::read_source()` sample when no broadcast arrived with it.
const WATCHDOG_TIMER: usize = 2;

/// Cadence of [`WATCHDOG_TIMER`] in seconds, kept as a plain integer so the
/// test below can assert the band in a `const` block : events +
/// a 1–2 s watchdog). Measured failure it answers: after a plug on
/// build 26200.9711 the OS delivered a stale `battery` broadcast and no AC
/// broadcast for 5 m 21 s — a broadcast-only listener then strands on AC
/// with no engine (B3). One wakeup and one `GetSystemPowerStatus` call per
/// 2 s is still zero compute by 's definition (no engine, no host,
/// no GPU).
const WATCHDOG_INTERVAL_SECS: u64 = 2;
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(WATCHDOG_INTERVAL_SECS);

/// A `settings.set` lifted the stop rule while this listener was serving
/// the pipe. The pipe thread records the
/// request here and posts a wake; the message loop consumes it on the next
/// iteration and hands off exactly like a confirmed AC. One process, one
/// pipe thread — a plain process-wide flag is the whole mechanism.
static START_REQUEST: AtomicBool = AtomicBool::new(false);

/// Record a hand-off request from the pipe thread (`pub(crate)` — the
/// listener's `settings.set` arm in `ipc.rs` is the only caller). Wake-only:
/// the post makes the main loop consume this now instead of at the next
/// 2 s watchdog tick; a stale post without the flag changes nothing.
pub(crate) fn request_full_start() {
    START_REQUEST.store(true, Ordering::Release);
    if let Some(hwnd) = control::find_existing() {
        control::post(hwnd, control::WM_APP_PIPE);
    }
}

/// Consume a pending request (one-shot).
fn take_full_start() -> bool {
    START_REQUEST.swap(false, Ordering::AcqRel)
}

/// Run the listener until asked to stop. Returns
/// [`RunExit::Exit`] when the control window closed (`cascade stop`,
/// window teardown), the pipe answered a `quit`, the tray chose Quit, or
/// handed the desktop back to a fresh full daemon — the listener never
/// asks the CLI to re-exec it; the full-start paths spawn their own child.
/// Returns `Ok(None)` on the
/// fail-open: the power subscription could not be established, so listener
/// mode must never begin and the caller falls through to the full daemon
///
///
/// Structural skip-load: the binary enters here
/// *before* `build_backend`, so no backend, factory call, or Media
/// Foundation session can exist in this process.
pub fn run(probe: ProbeFn) -> Result<Option<RunExit>, DaemonError> {
    // Declared first, therefore dropped last on every exit path — the same
    // guard as `daemon::run`.
    let _exit = daemon::ExitSignal;

    // One pipe, one control window: the same single-instance lock the full
    // daemon takes, so a listener and a daemon can
    // never coexist — whichever comes second gets `AlreadyRunning`.
    let pipe = daemon::create_session_pipe()?;
    daemon::install_panic_hook();
    // Kept alive for the whole run: dropping it would destroy the window
    // and take `find_existing()` down with it.
    let control = ControlWindow::create().map_err(DaemonError::Control)?;
    let hwnd = control.hwnd();

    // Entry order of: register → seed → drain, so a broadcast
    // recorded between the registration and the seed is picked up by the
    // drain instead of being missed.
    let mut power_state = match ListenerPower::new(hwnd) {
        Ok(state) => state,
        Err(code) => {
            // fail-open: without a live source
            // subscription this process could never transition back — so
            // listener mode must not begin at all. The pipe and window
            // created above are dropped on this return (the named pipe
            // instance releases; `ControlWindow`'s `Drop` destroys the
            // window and drains the `WM_QUIT` its wndproc posts — without
            // that the fall-through hit its own single-instance probe,
            // and `cmd_run` falls through to the full daemon, whose
            // own collector logs the same registration failure and stays
            // off. A process racing us in the release window gets an
            // honest `AlreadyRunning` — never two daemons.
            power::log_register_failure(code);
            derr!("listener: power registration failed; starting the full daemon instead");
            return Ok(None);
        }
    };

    let settings = offline::settings_get();
    let tray_want = settings.tray;

    // Channels mirror the daemon's shape: the first
    // snapshot travels before the thread starts, `commands` is never sent
    // on (the listener dispatches in place), and `flush_acked` carries the
    // `quit` handshake.
    let (msg_tx, msg_rx) = mpsc::channel();
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (flush_tx, flush_rx) = mpsc::channel();
    if msg_tx
        .send(ServerMsg::Snapshot(Arc::new(listening_snapshot(
            monitor::enumerate(),
            settings,
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
            // The listener answers `library.*` in place, so the
            // metadata probe rides the channel half; the daemon forwards
            // to its main loop instead and passes `None`.
            probe: Some(probe),
        },
        PipeRole::Listener,
    ) {
        // Same decision as the daemon: keep serving
        // what we can. The control window still answers `cascade stop`.
        derr!("ipc: could not start the pipe thread (error {code}); no `cascade status`");
    }

    // the tray stays. The icon must exist even
    // though nothing is playing — a listener nobody can see is a listener
    // nobody can quit.
    let mut tray = Tray::new(hwnd, tray_want, TrayMode::Listening);

    dlog!("listener: pipe, window, tray and power up; waiting (no engine by design)");

    // Blocked until a message arrives — zero wakeups in steady state
    // unlike the full daemon there is no host to watchdog, so the
    // 2 s tick and `MsgWaitForMultipleObjectsEx` are not needed at all.
    //
    // The first turn runs *before* the first `GetMessageW`: the source may
    // have returned to AC between the entry rule and the seed, and nothing
    // would wake this thread to notice it.
    let mut hand_off = power_state.turn(hwnd, false);
    let mut msg = MSG::default();
    loop {
        // a `settings.set` lifted the stop rule
        // while the pipe thread served it — hand off exactly like a
        // confirmed AC. Consumed at the top so a request recorded during
        // the previous drain is acted on before this thread sleeps again.
        if take_full_start() {
            dlog!("listener: on_battery is no longer stop; starting the full daemon");
            hand_off = true;
        }
        // confirmed AC → start the full child
        // and leave. A failed spawn keeps the listener running —
        // the latch inside `turn` makes the next attempt happen on the
        // next confirmed AC cycle, not on every wake, so `hand_off` is
        // simply re-decided after the next message.
        if hand_off && spawn_full_child() {
            break;
        }

        // SAFETY: `msg` is a valid out buffer; null hwnd + 0/0 filter
        // accepts every message posted to this thread (Win32 doc).
        let got = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if got <= 0 {
            // 0 = `WM_QUIT` (control window closed / `cascade stop`),
            // 1 = error: either way the listener ends. The desktop was
            // never touched, so there is nothing to repaint.
            break;
        }
        // SAFETY: `msg` was filled by `GetMessageW` above.
        unsafe { TranslateMessage(&msg) };
        // SAFETY: `msg` was filled by `GetMessageW` above.
        unsafe { DispatchMessageW(&msg) };

        if msg.message == control::WM_APP_PIPE {
            // Wake-only from the pipe thread: a
            // `quit` sends the flush ack *after* the client has its
            // answer, so collecting it here means the answer was written
            // and the listener may exit. A wake without the ack (a stale
            // post) changes nothing.
            if flush_rx.try_recv().is_ok() {
                control::request_quit();
            }
        }

        // One power turn per wake: consume a
        // broadcast the window procedure recorded (or, on a watchdog wake,
        // a fallback hardware read), keep the one-shot hysteresis timer
        // armed only while a candidate is pending, and decide. Only the
        // watchdog's `WM_TIMER` id is inspected — the hysteresis timer is
        // just another message that wakes this loop, which is the whole
        // reason it exists.
        let watchdog_due = msg.message == WM_TIMER && msg.wParam == WATCHDOG_TIMER;
        hand_off = power_state.turn(hwnd, watchdog_due);

        // Tray turn: reconcile the icon with the live
        // `settings.tray` and run a menu choice recorded during this
        // message drain. The listening menu offers `Start now` and
        // `Quit`.
        if let Some(action) = tray.step(tray_want, false) {
            match action {
                TrayAction::Quit => control::request_quit(),
                // hand the session over even
                // though the source is still battery. The spawn runs as
                // the arm's guard — a failed spawn simply doesn't
                // match, so the listener keeps running and the user can
                // choose the entry again.
                TrayAction::StartNow if spawn_start_now_child() => break,
                // `TrayMode::Listening` builds no entry that maps to the
                // full daemon's actions; ignore defensively instead
                // of guessing a behaviour.
                _ => {}
            }
        }

        if control::quit_requested() {
            break;
        }
    }

    // Exit ordering, same shape as the daemon's ladder: the icon leaves
    // first, then the subscription and the timer while their window is
    // still alive. There is no engine, no host and nothing to repaint —
    // the desktop still shows the real wallpaper.
    tray.remove();
    power_state.shutdown(hwnd);
    dlog!("listener: exiting");
    Ok(Some(RunExit::Exit))
}

/// Start the full child for. The argv is
/// this process's own minus `--video`, with a leading `--listen` rewritten
/// to `run`, plus `CASCADE_RESPAWN=1` — so the child retries the session
/// pipe this process still owns (`daemon::create_respawn_pipe`, 5 s
/// deadline) and then lands in `cmd_run`'s entry rules: full on AC,
/// listener again if the source turned out to be battery.
///
/// Returns whether the hand-off succeeded; a failure must leave the
/// listener running.
fn spawn_full_child() -> bool {
    match respawn::respawn_child() {
        Ok(pid) => {
            dlog!("listener: confirmed AC; full daemon is pid {pid}");
            true
        }
        Err(err) => {
            derr!("listener: could not start the full daemon ({err}); still listening");
            false
        }
    }
}

/// the tray's **Start now** — hand the session
/// over even though the source is still battery. The child carries
/// `CASCADE_SESSION_OVERRIDE` so it lands FULL with the
/// override instead of looping back into the listener, and it retries the
/// session pipe this process still owns (5 s deadline). A failed spawn
/// keeps the listener running — the user can simply choose the
/// entry again.
fn spawn_start_now_child() -> bool {
    match respawn::spawn_start_now_child() {
        // The pid is logged by the spawn helper itself.
        Ok(_) => true,
        Err(err) => {
            derr!("listener: could not start the full daemon ({err}); still listening");
            false
        }
    }
}

/// The listener's power half: the subscription, the
/// seeded hysteresis, the one-shot timer, and the latch. One
/// [`ListenerPower::turn`] per message wake — no polling loop :
/// events plus a watchdog, never a 60 Hz poll), and no timer in steady
/// state.
struct ListenerPower {
    /// `RegisterPowerSettingNotification` handle; `None` when the
    /// subscription failed (degraded, see [`ListenerPower::new`]).
    registration: Option<isize>,
    hysteresis: Hysteresis,
    /// Whether [`HYSTERESIS_TIMER`] is currently armed.
    timer_armed: bool,
    /// Whether [`WATCHDOG_TIMER`] is armed — for the whole run, not just
    /// while a candidate is pending.
    watchdog_armed: bool,
    /// Whether the current confirmed AC state has already been acted on.
    /// Cleared on every return to battery, which is what makes a failed
    /// spawn retry on the *next* AC event instead of on every wake.
    acted_on_ac: bool,
}

impl ListenerPower {
    /// Register → seed → drain, in that order.
    /// Returns the raw registration error when the subscription could not
    /// be established — the caller turns that into the fail-open
    /// instead of degrading into a listener that
    /// could never transition back.
    fn new(hwnd: HWND) -> Result<Self, u32> {
        // SAFETY: `hwnd` is our own control window, alive on this thread
        // for the whole run; the OS keeps posting `WM_POWERBROADCAST` to
        // it until [`ListenerPower::shutdown`]: a failure returns
        // before any seed or watchdog exists — no listener state may run
        // without a live subscription.
        let registration = Some(unsafe { power::register(hwnd) }?);
        let mut hysteresis = Hysteresis::default();
        // We only exist because the source *is* battery: publish it
        // directly rather than waiting out a candidate window. A candidate
        // would leave `confirmed` on AC for 3 s, and an AC event inside
        // that window would read as "already AC", cancel it, and strand
        // this listener with no transition back.
        hysteresis.seed(power::read_source().unwrap_or(true));
        // Anything the OS recorded between the two calls above: the newest
        // sample wins, so a flap in that gap is not lost.
        if let Some(raw) = power::take_raw() {
            hysteresis.observe(raw, Instant::now());
        }
        // The source watchdog: armed for the
        // whole run so a late, missing, or misvalued broadcast cannot
        // strand — every `WATCHDOG_INTERVAL` the loop wakes and may read
        // the source directly (see `turn`).
        // SAFETY: our own live control window on this thread; a null
        // procedure makes the system post `WM_TIMER` with `WATCHDOG_TIMER`
        // (Win32 doc for `SetTimer`).
        let watchdog_armed = unsafe {
            SetTimer(
                hwnd,
                WATCHDOG_TIMER,
                WATCHDOG_INTERVAL.as_millis() as u32,
                None,
            )
        } != 0;
        if !watchdog_armed {
            // Never fatal: broadcasts still work — would just
            // depend on them alone, exactly like the pre-amendment
            // listener that stranded in
            derr!("listener: source watchdog SetTimer failed; broadcasts only");
        }
        Ok(Self {
            registration,
            hysteresis,
            timer_armed: false,
            watchdog_armed,
            acted_on_ac: false,
        })
    }

    /// One power turn per message wake. Returns `true` when a confirmed AC
    /// state has not been acted on yet — the caller starts the full child
    /// and leaves.
    ///
    /// `watchdog_due` marks a [`WATCHDOG_TIMER`] wake: with no broadcast to
    /// consume it, the turn reads the source from hardware instead — the
    /// fallback that keeps a stale or missing broadcast from stranding the
    /// listener (amendment 2026-11-20, evidence ).
    fn turn(&mut self, hwnd: HWND, watchdog_due: bool) -> bool {
        let now = Instant::now();
        if let Some(raw) = power::take_raw() {
            self.hysteresis.observe(raw, now);
            dlog!(
                "listener: source sample {}",
                if raw { "battery" } else { "AC" }
            );
        } else if watchdog_due {
            // A difference from the confirmed state starts (or cancels) a
            // candidate exactly like a broadcast would — this is what
            // makes a missed broadcast recoverable instead of fatal. Only
            // a *different* sample is logged: the steady state stays
            // silent.
            if let Some(raw) = power::read_source() {
                if raw != self.hysteresis.confirmed() && !self.hysteresis.has_pending() {
                    dlog!(
                        "listener: watchdog source {} (no broadcast)",
                        if raw { "battery" } else { "AC" }
                    );
                }
                self.hysteresis.observe(raw, now);
            }
        }
        // Promotes a candidate that has been stable for 3 s; a no-op when
        // nothing is pending. The timer is only the wake that gets us here
        // on time.
        self.hysteresis.tick(now);

        // Arm only while a candidate is pending, kill the moment it
        // promotes or cancels — steady state therefore has zero timers
        //
        if self.hysteresis.has_pending() {
            if !self.timer_armed {
                // SAFETY: our own live control window on this thread; a
                // null procedure makes the system post `WM_TIMER` to it
                // (Win32 doc for `SetTimer`).
                if unsafe {
                    SetTimer(
                        hwnd,
                        HYSTERESIS_TIMER,
                        power::HYSTERESIS.as_millis() as u32,
                        None,
                    )
                } != 0
                {
                    self.timer_armed = true;
                } else {
                    // Rare (an invalid hwnd or a resource shortage): the
                    // next broadcast still wakes us, but a lone flap could
                    // wait for one. Never fatal.
                    derr!("listener: SetTimer failed; the 3 s window waits for the next message");
                }
            }
        } else if self.timer_armed {
            // SAFETY: the id came from our own successful `SetTimer` on
            // this same window.
            unsafe { KillTimer(hwnd, HYSTERESIS_TIMER) };
            self.timer_armed = false;
        }

        if self.hysteresis.confirmed() {
            // Still on battery: nothing to hand off, and a later AC cycle
            // is a fresh decision.
            self.acted_on_ac = false;
            return false;
        }
        // Confirmed AC. First time since the last battery state →.
        let first_time = !self.acted_on_ac;
        self.acted_on_ac = true;
        first_time
    }

    /// Drop the timers and the subscription while the window they point at
    /// is still alive — the same ordering as the daemon's exit ladder.
    fn shutdown(&mut self, hwnd: HWND) {
        if self.timer_armed {
            // SAFETY: our own window and our own timer id.
            unsafe { KillTimer(hwnd, HYSTERESIS_TIMER) };
            self.timer_armed = false;
        }
        if self.watchdog_armed {
            // SAFETY: the id came from our own successful `SetTimer` on
            // this same window, still alive here.
            unsafe { KillTimer(hwnd, WATCHDOG_TIMER) };
            self.watchdog_armed = false;
        }
        if let Some(handle) = self.registration.take() {
            power::unregister(handle);
        }
    }
}

/// The listener's status snapshot: honest about what
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
        // what the listener publishes — no engine, no host,
        // no playback, monitors listed with identity fields only.
        let snapshot = listening_snapshot(Vec::new(), Settings::default());
        assert_eq!(snapshot.mode, "listening");
        assert_eq!(snapshot.host_strategy, "none");
        assert_eq!(snapshot.repaint_level, 0);
        assert!(snapshot.playback.is_none());
        assert!(snapshot.paused_reasons.is_empty());
        assert!(snapshot.monitors.is_empty());
    }

    #[test]
    fn hysteresis_timer_id_stays_in_the_valid_range() {
        // `SetTimer`'s nIDEvent is a nonzero usize; 0 would be rejected
        // when combined with a null hwnd (never our case, but the id must
        // be stable so `KillTimer` targets the same one).
        const { assert!(HYSTERESIS_TIMER > 0, "timer ids must be nonzero") };
    }

    #[test]
    fn watchdog_timer_id_is_distinct_from_the_hysteresis_timer() {
        // The loop tells the two `WM_TIMER` wakes apart by id
        // a collision would feed a
        // fallback hardware read into every hysteresis tick — and worse,
        // `KillTimer` for one would silently kill the other.
        const { assert!(WATCHDOG_TIMER > 0, "timer ids must be nonzero") };
        const {
            assert!(
                WATCHDOG_TIMER != HYSTERESIS_TIMER,
                "watchdog and hysteresis timers must not collide"
            )
        };
        // The watchdog exists to be frequent enough to beat a stranded
        // broadcast; keep the cadence inside the 1–2 s band.
        const { assert!(WATCHDOG_INTERVAL_SECS >= 1, "watchdog slower than 1 s") };
        const {
            assert!(
                WATCHDOG_INTERVAL_SECS <= 2,
                "watchdog faster than the project rules allow"
            )
        };
    }
}
