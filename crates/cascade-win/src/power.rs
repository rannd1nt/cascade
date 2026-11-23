//! Power-source collector (slice ).
//!
//! Two halves:
//!
//! * **Pure logic** — [`Hysteresis`] turns raw AC/battery samples into a
//!   confirmed state that only changes after the source stayed stable for
//!   [`HYSTERESIS`] (3 s), and [`reconcile`] decides what `on_battery=stop`
//!   must do about the engine. Both are unit-tested without Win32.
//! * **Win32 edge** — registration (`RegisterPowerSettingNotification` for
//!   `GUID_ACDC_POWER_SOURCE`), one seed read (`GetSystemPowerStatus`), and
//!   the raw sample handoff from the control window's `WM_POWERBROADCAST`
//!   to the main loop (atomics, like the session-lock flags in
//!   [`crate::control`]).
//!
//! The collector exists **only while `on_battery != off`**: at the
//! default the daemon never calls `GetSystemPowerStatus` and never
//! subscribes to power events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cascade_core::rule_engine::BatteryAction;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM};
use windows_sys::Win32::System::Power::{
    GetSystemPowerStatus, RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
    POWERBROADCAST_SETTING, SYSTEM_POWER_STATUS,
};
use windows_sys::Win32::System::SystemServices::GUID_ACDC_POWER_SOURCE;
use windows_sys::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE;

use crate::{derr, dlog};

/// A raw source sample must stay unchanged for this long before the
/// collector publishes it: kills flapping (C4) and makes
/// a manual `resume` hold until the next *confirmed* transition (C3).
pub const HYSTERESIS: Duration = Duration::from_secs(3);

/// The confirmed state of the AC↔battery source.
///
/// Pure: samples and timestamps go in, the published state comes out. The
/// caller owns the instance (the daemon's main loop) and feeds it from
/// [`take_raw`] on every wake — nothing here reads hardware or sleeps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hysteresis {
    /// State visible to the pause oracle.
    confirmed: bool,
    /// A raw candidate and the time it was first seen.
    pending: Option<(bool, Instant)>,
}

impl Hysteresis {
    /// Feed one raw sample (an AC↔battery event or the seed read).
    ///
    /// A sample equal to the confirmed state cancels any pending candidate
    /// (the source came back); a *new* candidate starts its stability
    /// window; re-seeing the same candidate keeps its original anchor so a
    /// stream of duplicate events cannot postpone the promotion forever.
    pub fn observe(&mut self, raw: bool, now: Instant) {
        if raw == self.confirmed {
            self.pending = None;
        } else if !self.pending.is_some_and(|(value, _)| value == raw) {
            self.pending = Some((raw, now));
        }
    }

    /// Promote a candidate that has been stable for [`HYSTERESIS`].
    /// Returns the confirmed state.
    pub fn tick(&mut self, now: Instant) -> bool {
        let promote = match self.pending {
            Some((value, anchor)) if now.duration_since(anchor) >= HYSTERESIS => Some(value),
            _ => None,
        };
        if let Some(value) = promote {
            self.confirmed = value;
            self.pending = None;
        }
        self.confirmed
    }

    /// Confirmed state without advancing time (what `status` would show).
    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// True while a raw candidate is waiting out its stability window.
    /// The listener arms its one-shot `SetTimer` only in this state
    /// (steady state has zero timers).
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Publish `raw` as the confirmed state with no stability window
    /// The listener uses this instead of
    /// [`observe`](Self::observe): it only exists *because* the source is
    /// already on battery, so there is nothing to wait for — and a seeded
    /// candidate would leave `confirmed` on AC for the first 3 s, a window
    /// in which a real AC event would read as "already AC", cancel the
    /// candidate, and strand the listener with no transition back.
    pub fn seed(&mut self, raw: bool) {
        self.confirmed = raw;
        self.pending = None;
    }

    /// Drop everything: the collector was disabled or the daemon is
    /// resetting. The next enable seeds a fresh state from the hardware.
    pub fn reset(&mut self) {
        self.confirmed = false;
        self.pending = None;
    }
}

/// What the collector must do about the engine right now.
///
/// Pure so the unload/reload decision is unit-tested without a host
/// window. `Reload` never loads in this process: the daemon respawns so
/// the child gets flat memory (a second load pipeline
/// would retain ~50 MB until process exit).
///
/// The consumer of [`Reconcile::Unload`] changed in
/// `power_step` maps it to the existing `power_respawn` exit ladder
/// repaint the real wallpaper, respawn, and let the child land in the
/// battery listener — instead of an in-place unload. The pure meaning is
/// untouched: "the engine must go away".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconcile {
    /// Nothing to do; the rules may still need a re-evaluate.
    Nothing,
    /// `on_battery=stop` on battery with a loaded engine: it must go away.
    /// Consumed by `power_step` as an exit-ladder + respawn (
    /// never as an in-place unload.
    Unload,
    /// The video should be present but the engine is unloaded: respawn.
    Reload,
}

/// Decide the engine action from the confirmed source, the setting, and
/// the two pieces of state that uniquely describe the engine:
/// `loaded` (a validated source is bound) and `has_video`
/// (`state.video` — kept across an unload, cleared by `clear`/failure).
///
/// The pair `has_video && !loaded` occurs in exactly one legitimate
/// situation — an `on_battery=stop` unload — so `Reload` can never fight
/// a failed load or a cleared wallpaper into a respawn loop: every load
/// failure clears `state.video` before the next tick (`daemon.rs` startup
/// load, `wallpaper.set`, `wallpaper.clear`), and
/// makes the unload leave the process instead of flipping `loaded` in
/// place.
pub fn reconcile(
    on_battery: bool,
    action: BatteryAction,
    loaded: bool,
    has_video: bool,
) -> Reconcile {
    let stopping = on_battery && action == BatteryAction::Stop;
    if stopping && loaded {
        Reconcile::Unload
    } else if !stopping && has_video && !loaded {
        Reconcile::Reload
    } else {
        Reconcile::Nothing
    }
}

/// Raw AC/battery sample waiting for the main loop (`WM_POWERBROADCAST`
/// records it, [`take_raw`] consumes it). Same atomics pattern as the
/// session-lock flags: the window procedure never touches engine state.
static RAW: AtomicBool = AtomicBool::new(false);
static RAW_CHANGED: AtomicBool = AtomicBool::new(false);

/// Record a raw source sample from the window procedure (atomics only).
pub fn record_raw(on_battery: bool) {
    RAW.store(on_battery, Ordering::Release);
    RAW_CHANGED.store(true, Ordering::Release);
}

/// Consume the latest raw sample, or `None` when nothing arrived since
/// the last call. A sample that arrives while the collector is disabled
/// is consumed and dropped by the caller (it is stale by then anyway).
pub fn take_raw() -> Option<bool> {
    if RAW_CHANGED.swap(false, Ordering::AcqRel) {
        Some(RAW.load(Ordering::Acquire))
    } else {
        None
    }
}

/// Sentinel error [`register`] returns when the forced-failure hook is
/// set: a fake code no Win32 call would
/// ever produce, so the log line is obviously a test.
pub const REGISTER_FAIL_SENTINEL: u32 = 0xDEAD_0001;

/// Subscribe `hwnd` to AC↔DC power-source changes. Returns the
/// registration handle for [`unregister`].
///
/// # Safety
///
/// `hwnd` must be a live window created on the calling thread that will
/// keep pumping messages — the OS delivers `WM_POWERBROADCAST` to it
/// until [`unregister`] runs or the window is destroyed.
pub unsafe fn register(hwnd: HWND) -> Result<isize, u32> {
    // forced-failure hook (precedent `CASCADE_GATE`):
    // dev-only manual verification of the fail-open path — the listener
    // refuses to become a listener and the full daemon runs with the
    // collector off. Never set in production; no product behaviour depends
    // on this variable.
    if std::env::var_os("CASCADE_TEST_REGISTER_FAIL").is_some() {
        return Err(REGISTER_FAIL_SENTINEL);
    }
    // SAFETY: required by the signature above; the GUID is a const that
    // outlives the call; the flags value is the documented window-handle
    // variant.
    let handle = unsafe {
        RegisterPowerSettingNotification(hwnd, &GUID_ACDC_POWER_SOURCE, DEVICE_NOTIFY_WINDOW_HANDLE)
    };
    if handle == 0 {
        // SAFETY: reads the thread's last-error value.
        return Err(unsafe { GetLastError() });
    }
    Ok(handle)
}

/// Drop a registration from [`register`]. Unregistering twice or with a
/// foreign handle is a no-op at worst (the OS validates the handle).
pub fn unregister(handle: isize) {
    // SAFETY: the handle came from our own successful `register` and is
    // consumed exactly once by the caller.
    let _ = unsafe { UnregisterPowerSettingNotification(handle) };
}

/// The one seed read : only ever called while the user has
/// opted in). `None` means "no honest answer" — an unknown AC status
/// (255) or a failed call — and the collector then waits for the first
/// event instead of guessing.
pub fn read_source() -> Option<bool> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: out-pointer to a local that outlives the call.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return None;
    }
    match status.ACLineStatus {
        0 => Some(true),  // offline → on battery
        1 => Some(false), // online → on AC
        _ => None,        // 255 = unknown
    }
}

/// Compare a broadcast GUID against `GUID_ACDC_POWER_SOURCE`.
/// (windows-sys's `GUID` derives no `PartialEq`, hence field by field.)
pub fn is_acdc(guid: &GUID) -> bool {
    let expected = GUID_ACDC_POWER_SOURCE;
    guid.data1 == expected.data1
        && guid.data2 == expected.data2
        && guid.data3 == expected.data3
        && guid.data4 == expected.data4
}

/// Parse a `PBT_POWERSETTINGCHANGE` payload and record the AC/DC sample.
///
/// # Safety
/// `lparam` must point at the `POWERBROADCAST_SETTING` the OS attached to
/// this `WM_POWERBROADCAST` message.
pub unsafe fn record_broadcast(lparam: LPARAM) {
    // SAFETY: the caller guarantees lParam is the message's
    // POWERBROADCAST_SETTING, which the OS allocated with its Data bytes.
    let setting = unsafe { &*(lparam as *const POWERBROADCAST_SETTING) };
    if !is_acdc(&setting.PowerSetting) || setting.DataLength < 4 {
        return;
    }
    // SAFETY: DataLength >= 4 puts a full DWORD inside the message buffer
    // (GUID_ACDC_POWER_SOURCE carries a SYSTEM_POWER_CONDITION DWORD) and
    // read_unaligned tolerates the byte offset.
    let value = unsafe { (setting.Data.as_ptr() as *const u32).read_unaligned() };
    // Log every payload before parsing: a *dropped* value is invisible
    // downstream, and evidence (build 26200.9711) shows the OS can
    // deliver a stale or surprising value at a plug — the raw number is
    // what makes that diagnosable from a redirected stdout.
    match source_from_dword(value) {
        Some(sample) => {
            dlog!(
                "power: broadcast value={value} sample {}",
                if sample { "battery" } else { "AC" }
            );
            record_raw(sample);
        }
        None => dlog!(
            "power: broadcast value={value} ignored (DataLength={})",
            setting.DataLength
        ),
    }
}

/// The AC/DC payload mapping (learn.microsoft.com "Power Setting GUIDs":
/// the Data DWORD is a [`SYSTEM_POWER_CONDITION`] — PoAc, PoDc, PoHot —
/// *not* an `ACLineStatus`, a different enum where 0 means offline).
///
/// `PoHot` (UPS) counts as battery the same way the Microsoft sample does
/// (`m_bOnBattery = 0 != nPowerSrc`): it is not wall power. Anything
/// `>= PoConditionMaximum` is out of range and records nothing — an
/// unknown source must never move the pause oracle.
///
/// [`SYSTEM_POWER_CONDITION`]: https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-system_power_condition
fn source_from_dword(value: u32) -> Option<bool> {
    match value {
        0 => Some(false),    // PoAc: wall power
        1 | 2 => Some(true), // PoDc (battery) or PoHot (UPS)
        _ => None,
    }
}

/// Log a collector state change once, at the transition (the main loop
/// calls this only when the gate flips).
pub fn log_gate(enabled: bool, action: BatteryAction) {
    if enabled {
        dlog!("power: collector enabled (on_battery={})", action.as_str());
    } else {
        dlog!("power: collector disabled (on_battery=off)");
    }
}

/// Log a registration failure once (the caller stops retrying until the
/// setting is toggled off and on again).
pub fn log_register_failure(code: u32) {
    derr!("power: RegisterPowerSettingNotification failed ({code}); on-battery rules stay off");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn hysteresis_needs_three_stable_seconds_before_it_promotes() {
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        assert!(!hysteresis.confirmed());

        hysteresis.observe(true, t0);
        assert!(!hysteresis.tick(at(t0, 2_999)), "2.999 s is not stable yet");
        assert!(!hysteresis.confirmed(), "still the old state");
        assert!(hysteresis.tick(at(t0, 3_000)), "3 s promotes");
        assert!(hysteresis.confirmed());
        // Promoted: the pending candidate is gone, so ticking far ahead
        // changes nothing.
        assert!(hysteresis.tick(at(t0, 60_000)));
    }

    #[test]
    fn hysteresis_cancels_a_candidate_that_flaps_back() {
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        hysteresis.observe(true, t0);
        // Back to the confirmed source before the window elapsed (C4).
        hysteresis.observe(false, at(t0, 1_000));
        assert!(
            !hysteresis.tick(at(t0, 10_000)),
            "a flap must never promote"
        );
        assert!(!hysteresis.confirmed());
    }

    #[test]
    fn hysteresis_keeps_the_anchor_when_the_same_sample_repeats() {
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        hysteresis.observe(true, t0);
        // Duplicate events at +1 s and +2 s must not restart the window:
        // promotion happens 3 s after the FIRST sighting.
        hysteresis.observe(true, at(t0, 1_000));
        hysteresis.observe(true, at(t0, 2_000));
        assert!(!hysteresis.tick(at(t0, 2_999)), "anchor is still t0");
        assert!(hysteresis.tick(at(t0, 3_000)), "3 s after t0 promotes");
    }

    #[test]
    fn hysteresis_a_new_candidate_restarts_the_window() {
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        hysteresis.observe(true, t0);
        // Source flips the other way at +1 s: the `true` candidate dies
        // with its anchor and `false` equals the confirmed state anyway.
        hysteresis.observe(false, at(t0, 1_000));
        hysteresis.observe(true, at(t0, 2_000));
        assert!(
            !hysteresis.tick(at(t0, 4_000)),
            "the new true candidate needs its own 3 s (from +2 s)"
        );
        assert!(hysteresis.tick(at(t0, 5_000)));
    }

    #[test]
    fn hysteresis_seed_publishes_immediately_with_no_window() {
        // the listener seeds from `read_source()` — it
        // exists because the source is already battery, so a candidate
        // would leave `confirmed` on AC for 3 s and swallow a real AC
        // event inside that window (it would read as "already AC").
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        hysteresis.seed(true);
        assert!(hysteresis.confirmed(), "published without waiting");
        assert!(!hysteresis.has_pending(), "a seed is not a candidate");
        assert!(hysteresis.tick(t0), "the seeded state survives a tick");
        // A real AC event now has a full 3 s window of its own: `tick`
        // returns the *confirmed* state, which stays battery until the
        // window elapses.
        hysteresis.observe(false, t0);
        assert!(hysteresis.has_pending());
        assert!(
            hysteresis.tick(at(t0, 2_999)),
            "battery is still confirmed at 2.999 s"
        );
        assert!(
            !hysteresis.tick(at(t0, 3_000)),
            "AC is confirmed once the window elapsed"
        );
    }

    #[test]
    fn has_pending_tracks_only_the_candidate_window() {
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        assert!(!hysteresis.has_pending());
        hysteresis.observe(true, t0);
        assert!(hysteresis.has_pending(), "a new candidate is pending");
        // The source flaps back: the candidate dies with its window.
        hysteresis.observe(false, at(t0, 500));
        assert!(
            !hysteresis.has_pending(),
            "cancelled by the confirmed value"
        );
        // Promoting clears the candidate too.
        hysteresis.observe(true, t0);
        hysteresis.tick(at(t0, 3_000));
        assert!(!hysteresis.has_pending(), "promoted, nothing pending");
        hysteresis.reset();
        assert!(!hysteresis.has_pending());
    }

    #[test]
    fn hysteresis_reset_returns_to_the_disabled_state() {
        let t0 = Instant::now();
        let mut hysteresis = Hysteresis::default();
        hysteresis.observe(true, t0);
        assert!(hysteresis.tick(at(t0, 3_000)));
        hysteresis.reset();
        assert!(!hysteresis.confirmed(), "reset clears the confirmed state");
        assert_eq!(hysteresis.pending, None, "reset drops the candidate");
    }

    #[test]
    fn reconcile_unloads_only_stop_on_battery_with_a_loaded_engine() {
        assert_eq!(
            reconcile(true, BatteryAction::Stop, true, true),
            Reconcile::Unload
        );
        // Not on battery, or another action: the engine stays attached.
        assert_eq!(
            reconcile(false, BatteryAction::Stop, true, true),
            Reconcile::Nothing
        );
        assert_eq!(
            reconcile(true, BatteryAction::Pause, true, true),
            Reconcile::Nothing
        );
    }

    #[test]
    fn reconcile_reloads_whenever_the_video_should_be_back() {
        // AC return after a Stop-unload.
        assert_eq!(
            reconcile(false, BatteryAction::Stop, false, true),
            Reconcile::Reload
        );
        // The action changed to something that keeps the frame.
        assert_eq!(
            reconcile(true, BatteryAction::Pause, false, true),
            Reconcile::Reload
        );
        // The collector was switched off while unloaded.
        assert_eq!(
            reconcile(false, BatteryAction::Off, false, true),
            Reconcile::Reload
        );
    }

    #[test]
    fn reconcile_never_reloads_a_video_that_does_not_exist() {
        // No video ever / cleared / failed load: `has_video` is false, so
        // no respawn loop is possible.
        assert_eq!(
            reconcile(false, BatteryAction::Stop, false, false),
            Reconcile::Nothing
        );
        assert_eq!(
            reconcile(true, BatteryAction::Pause, false, false),
            Reconcile::Nothing
        );
        // Loaded with nothing persisted: `on_battery=stop` still unloads —
        // the setting says "no engine on battery" — and the reload branch
        // below needs `has_video`, so the engine cannot come back and loop.
        assert_eq!(
            reconcile(true, BatteryAction::Stop, true, false),
            Reconcile::Unload
        );
        // After that unload: stable, no respawn (nothing to respawn with).
        assert_eq!(
            reconcile(true, BatteryAction::Stop, false, false),
            Reconcile::Nothing
        );
    }

    #[test]
    fn reconcile_is_stable_while_staying_on_battery() {
        // After the unload the engine is gone and the source is still
        // battery: every later tick must be a no-op (no reload loop).
        assert_eq!(
            reconcile(true, BatteryAction::Stop, false, true),
            Reconcile::Nothing
        );
    }

    #[test]
    fn raw_samples_coalesce_to_the_latest() {
        // The only test touching the module statics (same rule as the
        // session-lock test in `control`).
        let _ = take_raw();
        assert_eq!(take_raw(), None);
        record_raw(true);
        record_raw(false);
        // Two records between takes: the latest state wins, once.
        assert_eq!(take_raw(), Some(false));
        assert_eq!(take_raw(), None);
    }

    #[test]
    fn acdc_guid_is_recognised_and_others_are_not() {
        assert!(is_acdc(&GUID_ACDC_POWER_SOURCE));
        let other = GUID::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        assert!(!is_acdc(&other));
    }

    #[test]
    fn acdc_payload_is_system_power_condition_not_ac_linestatus() {
        // Regression (observed on build 26200, 2026-11-08): the payload is
        // a SYSTEM_POWER_CONDITION (PoAc=0, PoDc=1, PoHot=2). Reading it
        // like ACLineStatus (0 = offline) inverted every sample and the
        // daemon paused "on_battery" while plugged in.
        assert_eq!(source_from_dword(0), Some(false), "PoAc: wall power");
        assert_eq!(source_from_dword(1), Some(true), "PoDc: battery");
        assert_eq!(
            source_from_dword(2),
            Some(true),
            "PoHot: UPS is not wall power"
        );
        assert_eq!(
            source_from_dword(3),
            None,
            "PoConditionMaximum: out of range"
        );
        assert_eq!(source_from_dword(255), None, "garbage records nothing");
    }
}
