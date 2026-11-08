//! Pure RuleEngine: SessionState in, DesiredPlayback out.
//!
//! No Win32 inside. All pause/play decisions are made here based on
//! session state + user settings. The daemon (cascade-win) feeds
//! SessionState from Win32 events + power notifications and calls the
//! engine on every watchdog tick / state change.
//!
//! See `docs/architecture.md` §7 for the pause contract.

use crate::PauseReason;

/// Power profile selected by the user (`settings set power_profile`,
/// design-m4 D8/D9).
///
/// `Off` (the default) means Cascade applies no profile at all: the daemon
/// reads no power state and nothing here changes playback. A profile is a
/// **preset** that fills `on_battery` while it is still `off` — it is never
/// an independent pause source (design-m4 §2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerProfile {
    /// No profile: default behaviour, only the pause rules apply (D8).
    #[default]
    Off,
    /// Pause on fullscreen/lock/display-off, uncapped fps on AC.
    Balanced,
    /// Power saving preset: suggests `on_battery = pause`.
    Battery,
    /// Never silently reduce quality or pause for power (advanced).
    Quality,
}

impl PowerProfile {
    /// Parse the wire/CLI spelling. Case-insensitive.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "balanced" => Some(Self::Balanced),
            "battery" => Some(Self::Battery),
            "quality" => Some(Self::Quality),
            _ => None,
        }
    }

    /// Must match the serde spelling (`ipc.md` §5).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Balanced => "balanced",
            Self::Battery => "battery",
            Self::Quality => "quality",
        }
    }
}

/// What to do when running on battery (`settings set on_battery`,
/// design-m4 D8/D9).
///
/// `Off` (the default) means the daemon never even reads the power source
/// (D8: no `GetSystemPowerStatus` until the user opts in). When it is
/// enabled, the collector is event-driven — a power-source transition with
/// ~3 s hysteresis, never a per-tick re-evaluation — so a manual
/// `cascade resume` holds until the next transition (design-m4 §2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryAction {
    /// Ignore power state entirely; never read it (D8 default).
    #[default]
    Off,
    /// Pause the video, freeze the last frame on the desktop.
    Pause,
    /// Stop playback entirely (unload). Until the power collector lands
    /// (design-m4 S2b) this behaves like `Pause`; both report
    /// `PauseReason::OnBattery`.
    Stop,
    /// Keep playing on battery.
    Continue,
}

impl BatteryAction {
    /// Parse the wire/CLI spelling. Case-insensitive.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "pause" => Some(Self::Pause),
            "stop" => Some(Self::Stop),
            "continue" => Some(Self::Continue),
            _ => None,
        }
    }

    /// Must match the serde spelling (`ipc.md` §5).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pause => "pause",
            Self::Stop => "stop",
            Self::Continue => "continue",
        }
    }
}

/// Which pause reasons are enabled (user settings, `settings set pause.*`).
///
/// The `Default` is the documented product default (`ipc.md` §5), so a
/// missing `[pause]` table or key in `config.toml` decodes to it
/// (`#[serde(default)]` on the container).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PauseSettings {
    pub fullscreen: bool,
    pub maximized: bool,
    pub display_off: bool,
    pub session_lock: bool,
    pub screensaver: bool,
    pub remote_session: bool,
}

impl Default for PauseSettings {
    fn default() -> Self {
        Self {
            fullscreen: true,
            maximized: false,
            display_off: true,
            session_lock: true,
            screensaver: true,
            remote_session: true,
        }
    }
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
    pub power_profile: PowerProfile, // Preset only (D9): never pauses by itself
    pub on_battery: bool,     // True when the power source is battery (S2b collector)
    pub pause_settings: PauseSettings, // Per-reason toggles
    pub battery_action: BatteryAction, // What to do on battery (D8: default Off)
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
/// One oracle, first match wins (design-m4 §2.6, D9):
/// 1. `user_paused` — manual override, always wins; a manual `resume`
///    holds until the condition that paused it changes
/// 2. `degraded` — host/device unhealthy, pause but keep desktop valid
/// 3. Enabled pause reasons (fullscreen, maximized, display_off, ...)
/// 4. Battery action — event-driven, reports `PauseReason::OnBattery`
/// 5. `playing` — default
///
/// `power_profile` never enters this function: it is a settings-layer
/// preset (it fills `on_battery` while unset) and cannot pause on its own,
/// so the two systems cannot fight each other (design-m4 §2.6).
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

        // Battery handling (D8 default `Off`: the collector never reads the
        // power source, so `on_battery` stays false and this is unreachable
        // until the user opts in). Pause and stop both freeze playback and
        // report the real reason (design-m4 §2.6 C7).
        if state.on_battery {
            match state.battery_action {
                BatteryAction::Pause | BatteryAction::Stop => {
                    return DesiredPlayback::paused(PauseReason::OnBattery);
                }
                BatteryAction::Off | BatteryAction::Continue => {}
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
    fn battery_off_is_the_d8_default_and_never_reads_power() {
        // D8: default off — the collector stays dark until the user opts in.
        assert_eq!(BatteryAction::default(), BatteryAction::Off);
        let state = SessionState {
            on_battery: true, // would only happen after the S2b collector is enabled
            ..SessionState::default()
        };
        let result = RuleEngine::evaluate(state);
        assert!(result.playing, "Off must ignore the battery state entirely");
    }

    #[test]
    fn battery_pause_and_stop_report_on_battery() {
        // Design-m4 §2.6 C7: battery pause must not masquerade as
        // display_off or user_paused.
        for action in [BatteryAction::Pause, BatteryAction::Stop] {
            let state = SessionState {
                on_battery: true,
                battery_action: action,
                ..SessionState::default()
            };
            let result = RuleEngine::evaluate(state);
            assert!(!result.playing, "{action:?} must pause");
            assert_eq!(
                result.pause_reason,
                Some(PauseReason::OnBattery),
                "{action:?} must report the real reason"
            );
        }
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
    fn power_profile_default_is_off() {
        // D8: no profile until the user opts in.
        assert_eq!(PowerProfile::default(), PowerProfile::Off);
    }

    #[test]
    fn power_profile_and_battery_action_wire_strings_are_stable() {
        // Renaming these breaks config.toml and every client (ipc.md §5).
        for (profile, wire) in [
            (PowerProfile::Off, "off"),
            (PowerProfile::Balanced, "balanced"),
            (PowerProfile::Battery, "battery"),
            (PowerProfile::Quality, "quality"),
        ] {
            assert_eq!(profile.as_str(), wire);
            assert_eq!(PowerProfile::parse(wire), Some(profile));
            let json = serde_json::to_string(&profile).expect("serialise");
            assert_eq!(json, format!("\"{wire}\""));
            let back: PowerProfile = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, profile);
        }
        for (action, wire) in [
            (BatteryAction::Off, "off"),
            (BatteryAction::Pause, "pause"),
            (BatteryAction::Stop, "stop"),
            (BatteryAction::Continue, "continue"),
        ] {
            assert_eq!(action.as_str(), wire);
            assert_eq!(BatteryAction::parse(wire), Some(action));
            let json = serde_json::to_string(&action).expect("serialise");
            assert_eq!(json, format!("\"{wire}\""));
            let back: BatteryAction = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, action);
        }
        // Parsing is forgiving about case and space, strict about nonsense.
        assert_eq!(
            PowerProfile::parse(" Quality "),
            Some(PowerProfile::Quality)
        );
        assert_eq!(BatteryAction::parse("PAUSE"), Some(BatteryAction::Pause));
        assert_eq!(PowerProfile::parse("turbo"), None);
        assert_eq!(BatteryAction::parse("suspend"), None);
    }

    #[test]
    fn pause_settings_default_matches_the_documented_defaults() {
        // ipc.md §5: maximized off, everything else on. This Default is what
        // a missing [pause] table in config.toml decodes to.
        let default = PauseSettings::default();
        assert!(default.fullscreen);
        assert!(!default.maximized);
        assert!(default.display_off);
        assert!(default.session_lock);
        assert!(default.screensaver);
        assert!(default.remote_session);
        // A partial [pause] table fills missing keys from the same defaults.
        let partial: PauseSettings =
            serde_json::from_str(r#"{"maximized":true}"#).expect("partial pause table");
        assert!(partial.maximized);
        assert!(partial.fullscreen, "missing keys fall back to the default");
    }
}
