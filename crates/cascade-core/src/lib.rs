//! Platform-independent types for Cascade.
//!
//! Nothing in this crate may depend on Win32, Media Foundation, or a GUI toolkit.

pub const PROTOCOL_VERSION: u32 = 1;

/// Unique identifier for a monitor, e.g. `\\.\DISPLAY1`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MonitorId(pub String);

impl MonitorId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Rectangle in virtual-desktop coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// Why playback is currently paused. Empty means "playing".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    Fullscreen,
    Maximized,
    DisplayOff,
    SessionLocked,
    Screensaver,
    RemoteSession,
    UserPaused,
    Degraded,
}

impl PauseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Maximized => "maximized",
            Self::DisplayOff => "display_off",
            Self::SessionLocked => "session_locked",
            Self::Screensaver => "screensaver",
            Self::RemoteSession => "remote_session",
            Self::UserPaused => "user_paused",
            Self::Degraded => "degraded",
        }
    }
}

/// High-level engine mode, used by IPC/status and the watchdog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    Boot,
    Attached,
    Paused,
    Degraded,
    SafeMode,
    Stopped,
}

impl EngineMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Attached => "attached",
            Self::Paused => "paused",
            Self::Degraded => "degraded",
            Self::SafeMode => "safe_mode",
            Self::Stopped => "stopped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_new_keeps_fields() {
        let r = Rect::new(10, 20, 1366, 768);
        assert_eq!((r.x, r.y, r.w, r.h), (10, 20, 1366, 768));
    }

    #[test]
    fn rect_is_empty_edges() {
        assert!(!Rect::new(0, 0, 1, 1).is_empty());
        assert!(Rect::new(0, 0, 0, 768).is_empty());
        assert!(Rect::new(0, 0, 1366, 0).is_empty());
        assert!(Rect::new(0, 0, -1, 100).is_empty());
        assert!(Rect::new(0, 0, 100, -1).is_empty());
        assert!(Rect::default().is_empty());
    }

    #[test]
    fn pause_reason_strings_are_stable() {
        // These strings appear in `status` output and the IPC payload;
        // changing one is a breaking protocol change (docs/ipc.md).
        assert_eq!(PauseReason::Fullscreen.as_str(), "fullscreen");
        assert_eq!(PauseReason::Maximized.as_str(), "maximized");
        assert_eq!(PauseReason::DisplayOff.as_str(), "display_off");
        assert_eq!(PauseReason::SessionLocked.as_str(), "session_locked");
        assert_eq!(PauseReason::Screensaver.as_str(), "screensaver");
        assert_eq!(PauseReason::RemoteSession.as_str(), "remote_session");
        assert_eq!(PauseReason::UserPaused.as_str(), "user_paused");
        assert_eq!(PauseReason::Degraded.as_str(), "degraded");
    }

    #[test]
    fn engine_mode_strings_are_stable() {
        assert_eq!(EngineMode::Boot.as_str(), "boot");
        assert_eq!(EngineMode::Attached.as_str(), "attached");
        assert_eq!(EngineMode::Paused.as_str(), "paused");
        assert_eq!(EngineMode::Degraded.as_str(), "degraded");
        assert_eq!(EngineMode::SafeMode.as_str(), "safe_mode");
        assert_eq!(EngineMode::Stopped.as_str(), "stopped");
    }

    #[test]
    fn monitor_id_exposes_inner_str() {
        let id = MonitorId(String::from(r"\\.\DISPLAY1"));
        assert_eq!(id.as_str(), r"\\.\DISPLAY1");
    }

    #[test]
    fn protocol_version_is_one() {
        // Bumping this without a docs/ipc.md update breaks old clients.
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
