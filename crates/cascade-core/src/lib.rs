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
