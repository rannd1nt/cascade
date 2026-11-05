//! Pure RuleEngine: SessionState in, DesiredPlayback out.
//!
//! No Win32 inside. All pause/play decisions are made here based on
//! session state + user settings. The daemon (cascade-win) feeds
//! SessionState from Win32 events + power notifications and calls the
//! engine on every watchdog tick / state change.
//!
//! See `docs/architecture.md` §7 for the pause contract.

use crate::PauseReason;

/// Power profile selected by the user (GUI setting, M5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PowerProfile {
    #[default]
    Balanced, // Default: pause on fullscreen/lock/display-off, 60 fps on AC
    Battery, // "hemat" / power saving: more aggressive pausing
    Quality, // Never silently reduce quality/pause for power
}

/// What to do when running on battery (user setting, GUI M5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BatteryAction {
    #[default]
    Pause, // Pause video, keep desktop painted (freeze)
    Stop,     // Stop video entirely (auto-stop wallpaper)
    Continue, // Ignore battery state, keep playing
}

/// Which pause reasons are enabled (user settings, GUI M5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PauseSettings {
    pub fullscreen: bool,
    pub maximized: bool,
    pub display_off: bool,
    pub session_lock: bool,
    pub screensaver: bool,
    pub remote_session: bool,
}

impl PauseSettings {
    /// All enabled: the safest default for a wallpaper that should never
    /// interfere with the user.
    pub fn all_enabled() -> Self {
        Self {
            fullscreen: true,
            maximized: true,
            display_off: true,
            session_lock: true,
            screensaver: true,
            remote_session: true,
        }
    }

    /// None enabled: never auto-pause (user accepts any interference).
    pub fn none() -> Self {
        Self {
            fullscreen: false,
            maximized: false,
            display_off: false,
            session_lock: false,
            screensaver: false,
            remote_session: false,
        }
    }
}

/// All conditions that affect pause/play decisions.
/// Filled by the daemon from Win32 events + power notifications.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionState {
    pub fullscreen: bool,     // SHQueryUserNotificationState / foreground check
    pub maximized: bool,      // IsZoomed + rect vs monitor
    pub display_off: bool,    // GUID_CONSOLE_DISPLAY_STATE
    pub session_locked: bool, // WTSRegisterSessionNotification
    pub screensaver: bool,    // SPI_GETSCREENSAVERRUNNING
    pub remote_session: bool, // GetSystemMetrics(SM_REMOTESESSION)
    pub user_paused: bool,    // Manual pause from CLI / GUI / tray
    pub degraded: bool,       // Host / device unhealthy (watchdog)
    pub power_profile: PowerProfile, // User-selected profile
    pub on_battery: bool,     // GUID_ACDC_POWER_SOURCE + GetSystemPowerStatus
    pub pause_settings: PauseSettings, // Per-reason toggles
    pub battery_action: BatteryAction, // What to do on battery
}

/// What the engine wants playback to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredPlayback {
    pub playing: bool,
    pub pause_reason: Option<PauseReason>,
}

impl DesiredPlayback {
    pub fn playing() -> Self {
        Self {
            playing: true,
            pause_reason: None,
        }
    }

    pub fn paused(reason: PauseReason) -> Self {
        Self {
            playing: false,
            pause_reason: Some(reason),
        }
    }
}

/// Pure rule engine. Given session state, returns desired playback.
///
/// Priority order (first match wins):
/// 1. `user_paused` — manual override, always wins
/// 2. `degraded` — host/device unhealthy, pause but keep desktop valid
/// 3. Enabled pause reasons (fullscreen, maximized, display_off, etc.)
/// 4. Battery action — depends on power profile + user battery action
/// 5. `playing` — default
pub struct RuleEngine;

impl RuleEngine {
    /// Evaluate the current session state and return desired playback.
    pub fn evaluate(state: SessionState) -> DesiredPlayback {
        // Manual pause overrides everything.
        if state.user_paused {
            return DesiredPlayback::paused(PauseReason::UserPaused);
        }

        // Degraded host/device: pause but keep desktop valid.
        if state.degraded {
            return DesiredPlayback::paused(PauseReason::Degraded);
        }

        // Enabled pause reasons, in priority order.
        if state.fullscreen && state.pause_settings.fullscreen {
            return DesiredPlayback::paused(PauseReason::Fullscreen);
        }
        if state.maximized && state.pause_settings.maximized {
            return DesiredPlayback::paused(PauseReason::Maximized);
        }
        if state.display_off && state.pause_settings.display_off {
            return DesiredPlayback::paused(PauseReason::DisplayOff);
        }
        if state.session_locked && state.pause_settings.session_lock {
            return DesiredPlayback::paused(PauseReason::SessionLocked);
        }
        if state.screensaver && state.pause_settings.screensaver {
            return DesiredPlayback::paused(PauseReason::Screensaver);
        }
        if state.remote_session && state.pause_settings.remote_session {
            return DesiredPlayback::paused(PauseReason::RemoteSession);
        }

        // Battery handling: depends on power profile + user battery action.
        if state.on_battery {
            match state.battery_action {
                BatteryAction::Stop => {
                    return DesiredPlayback::paused(PauseReason::UserPaused);
                }
                BatteryAction::Pause => {
                    return DesiredPlayback::paused(PauseReason::DisplayOff);
                }
                BatteryAction::Continue => {}
            }
        }

        DesiredPlayback::playing()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_paused_overrides_everything() {
        let state = SessionState {
            user_paused: true,
            fullscreen: true,
            maximized: true,
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(!result.playing);
        assert_eq!(result.pause_reason, Some(PauseReason::UserPaused));
    }

    #[test]
    fn degraded_pauses_but_keeps_desktop() {
        let state = SessionState {
            degraded: true,
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(!result.playing);
        assert_eq!(result.pause_reason, Some(PauseReason::Degraded));
    }

    #[test]
    fn fullscreen_pauses_when_enabled() {
        let state = SessionState {
            fullscreen: true,
            pause_settings: PauseSettings {
                fullscreen: true,
                ..PauseSettings::default()
            },
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(!result.playing);
        assert_eq!(result.pause_reason, Some(PauseReason::Fullscreen));
    }

    #[test]
    fn fullscreen_does_not_pause_when_disabled() {
        let state = SessionState {
            fullscreen: true,
            pause_settings: PauseSettings {
                fullscreen: false,
                ..PauseSettings::default()
            },
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(result.playing);
    }

    #[test]
    fn maximized_pauses_when_enabled() {
        let state = SessionState {
            maximized: true,
            pause_settings: PauseSettings {
                maximized: true,
                ..PauseSettings::default()
            },
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(!result.playing);
        assert_eq!(result.pause_reason, Some(PauseReason::Maximized));
    }

    #[test]
    fn maximized_does_not_pause_when_disabled() {
        let state = SessionState {
            maximized: true,
            pause_settings: PauseSettings {
                maximized: false,
                ..PauseSettings::default()
            },
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(result.playing);
    }

    #[test]
    fn battery_stop_pauses() {
        let state = SessionState {
            on_battery: true,
            battery_action: BatteryAction::Stop,
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(!result.playing);
    }

    #[test]
    fn battery_pause_pauses() {
        let state = SessionState {
            on_battery: true,
            battery_action: BatteryAction::Pause,
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(!result.playing);
    }

    #[test]
    fn battery_continue_ignores_battery() {
        let state = SessionState {
            on_battery: true,
            battery_action: BatteryAction::Continue,
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(result.playing);
    }

    #[test]
    fn all_conditions_false_plays() {
        let state = SessionState::default();
        let result = RuleEngine::evaluate(state);
        assert!(result.playing);
        assert_eq!(result.pause_reason, None);
    }

    #[test]
    fn pause_settings_all_enabled() {
        let s = PauseSettings::all_enabled();
        assert!(s.fullscreen);
        assert!(s.maximized);
        assert!(s.display_off);
        assert!(s.session_lock);
        assert!(s.screensaver);
        assert!(s.remote_session);
    }

    #[test]
    fn pause_settings_none() {
        let s = PauseSettings::none();
        assert!(!s.fullscreen);
        assert!(!s.maximized);
        assert!(!s.display_off);
        assert!(!s.session_lock);
        assert!(!s.screensaver);
        assert!(!s.remote_session);
    }

    #[test]
    fn power_profile_default_is_balanced() {
        assert_eq!(PowerProfile::default(), PowerProfile::Balanced);
    }

    #[test]
    fn battery_action_default_is_pause() {
        assert_eq!(BatteryAction::default(), BatteryAction::Pause);
    }
}
