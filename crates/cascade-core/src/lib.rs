//! Platform-independent types for Cascade.
//!
//! Nothing in this crate may depend on Win32, Media Foundation, or a GUI toolkit.

use std::path::Path;

pub const PROTOCOL_VERSION: u32 = 1;

pub mod ipc;
pub mod rule_engine;

/// Unique identifier for a monitor, e.g. `\\.\DISPLAY1`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MonitorId(pub String);

impl MonitorId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Rectangle in virtual-desktop coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
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

/// How a video frame is fitted into the host window (`architecture.md` §6).
/// M2 serves `Contain` only; `Cover`/`Stretch` are Tier 2 (`quality.md` §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Cover,
    Contain,
    Stretch,
}

/// What the caller wants from a backend for one loaded source
/// (`architecture.md` §6).
#[derive(Clone, Debug)]
pub struct PlaybackOptions {
    pub fit: Fit,
    pub looping: bool,
    pub max_fps: Option<u32>,
    pub power_saver: bool,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        // M2 defaults (design-m2 §5): contain-only, loop on, uncapped fps,
        // no power profiles.
        Self {
            fit: Fit::Contain,
            looping: true,
            max_fps: None,
            power_saver: false,
        }
    }
}

/// Backend counters. Surfaced to `status` in M4; M2 measures externally
/// with GPU/CPU counters instead (`design-m2.md` §5).
#[derive(Clone, Debug, Default)]
pub struct BackendStats {
    pub hardware_decode: bool,
    pub decoded_frames: u64,
    pub dropped_frames: u64,
    pub video_size: (u32, u32),
    pub fps: f32,
}

/// The only win → media boundary (AGENTS §4). Core stays Win32-free: the
/// HWND travels as a plain integer.
#[derive(Clone, Debug)]
pub struct HostSurface {
    pub hwnd: isize,
    pub monitor: MonitorId,
    /// Virtual-desktop rect of that monitor.
    pub rect: Rect,
    pub dpi: u32,
}

/// Errors a media backend can report (design-m2 §2.1).
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("cannot open {path}: {reason}")]
    Open { path: String, reason: String },
    #[error("unsupported stream: {0}")]
    Unsupported(String),
    #[error("media foundation: {0}")]
    Mf(String),
    #[error("no surface attached")]
    NoSurface,
}

pub type BackendResult<T> = Result<T, BackendError>;

/// The win → media boundary as a trait: `cascade-win` calls it and knows
/// nothing about Media Foundation; `cascade-media` implements it and never
/// depends on `cascade-win` (`architecture.md` §6, design-m2 §2).
pub trait MediaBackend {
    fn load(&mut self, source: &Path, opts: &PlaybackOptions) -> BackendResult<()>;
    fn play(&mut self) -> BackendResult<()>;
    fn pause(&mut self) -> BackendResult<()>;
    fn set_looping(&mut self, on: bool) -> BackendResult<()>;
    fn stats(&self) -> BackendStats;
    fn attach_surface(&mut self, surface: HostSurface) -> BackendResult<()>;
    fn detach_surface(&mut self) -> BackendResult<()>;
    fn shutdown(self: Box<Self>);
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

    #[test]
    fn playback_defaults_match_m2_decisions() {
        let opts = PlaybackOptions::default();
        assert_eq!(opts.fit, Fit::Contain);
        assert!(opts.looping);
        assert_eq!(opts.max_fps, None);
        assert!(!opts.power_saver);
    }

    #[test]
    fn backend_error_messages_are_actionable() {
        let open = BackendError::Open {
            path: "C:\\videos\\clip.mp4".to_string(),
            reason: "not found".to_string(),
        };
        assert_eq!(
            open.to_string(),
            "cannot open C:\\videos\\clip.mp4: not found"
        );
        assert_eq!(
            BackendError::Unsupported("av1".to_string()).to_string(),
            "unsupported stream: av1"
        );
        assert_eq!(
            BackendError::Mf("0xC00D36B4".to_string()).to_string(),
            "media foundation: 0xC00D36B4"
        );
        assert_eq!(BackendError::NoSurface.to_string(), "no surface attached");
    }

    #[test]
    fn host_surface_carries_plain_integer_hwnd() {
        let surface = HostSurface {
            hwnd: 0x20344,
            monitor: MonitorId(String::from(r"\\.\DISPLAY1")),
            rect: Rect::new(0, 0, 1366, 768),
            dpi: 96,
        };
        assert_eq!(surface.hwnd, 0x20344);
        assert_eq!(surface.monitor.as_str(), r"\\.\DISPLAY1");
        assert_eq!((surface.rect.w, surface.rect.h), (1366, 768));
        assert_eq!(surface.dpi, 96);
    }

    #[test]
    fn media_backend_is_object_safe() {
        struct Stub;

        impl MediaBackend for Stub {
            fn load(&mut self, _source: &Path, _opts: &PlaybackOptions) -> BackendResult<()> {
                Ok(())
            }
            fn play(&mut self) -> BackendResult<()> {
                Ok(())
            }
            fn pause(&mut self) -> BackendResult<()> {
                Ok(())
            }
            fn set_looping(&mut self, _on: bool) -> BackendResult<()> {
                Ok(())
            }
            fn stats(&self) -> BackendStats {
                BackendStats::default()
            }
            fn attach_surface(&mut self, _surface: HostSurface) -> BackendResult<()> {
                Err(BackendError::NoSurface)
            }
            fn detach_surface(&mut self) -> BackendResult<()> {
                Ok(())
            }
            fn shutdown(self: Box<Self>) {}
        }

        let mut backend: Box<dyn MediaBackend> = Box::new(Stub);
        assert_eq!(backend.stats().decoded_frames, 0);
        let surface = HostSurface {
            hwnd: 0,
            monitor: MonitorId(String::from(r"\\.\DISPLAY1")),
            rect: Rect::default(),
            dpi: 96,
        };
        assert!(backend.attach_surface(surface).is_err());
        backend.shutdown();
    }
}
