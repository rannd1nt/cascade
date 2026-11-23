//! Platform-independent types for Cascade.
//!
//! Nothing in this crate may depend on Media Foundation or a GUI toolkit.
//! The single Win32 exception is [`client`] — the named-pipe **client**
//! transport: a narrow set of `windows-sys` calls so the
//! CLI and the GUI can both reach the daemon without depending on
//! `cascade-win`. The pipe server stays in `cascade-win`.

use std::path::Path;

pub const PROTOCOL_VERSION: u32 = 1;

pub mod assess;
pub mod client;
pub mod device;
pub mod ipc;
pub mod library;
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
    /// Running on battery with `on_battery` set to `pause`/`stop`:
    /// reported honestly instead of masquerading as `display_off` or
    /// `user_paused`.
    OnBattery,
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
            Self::OnBattery => "on_battery",
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

/// How a video frame is fitted into the host window.
/// serves `Contain` only; `Cover`/`Stretch` are Tier 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Cover,
    Contain,
    Stretch,
}

/// Which presentation route the backend builds at load time
/// (`settings set engine_mode`).
///
/// Named `PresentationMode` to stay distinct from the lifecycle
/// [`EngineMode`] (Boot/Attached/…). This is the **requested** mode: read
/// once, when the engine is created, and never re-read on a running engine
/// (no code path reconfigures a live engine). The **actual**
/// route is reported separately by `status.monitors[].engine`
/// (`frame-server` | `rendering`), because the accurate route keeps its
/// one-way fallback to rendering and the dev knob `CASCADE_ENGINE_RENDER=1`
/// forces rendering without touching this setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationMode {
    /// Frame-server + CPU staging bridge: correct colour on BT.601 content,
    /// at the measured cost of ~33.50% CPU / ~225 MB on the target machine
    ///
    #[default]
    Accurate,
    /// Rendering/EVR route: much cheaper (~9.00–10.32% CPU / ~121 MB) with
    /// the documented wrong colour on BT.601 content — the trade the user
    /// opts into, never a silent downgrade.
    Efficient,
}

impl PresentationMode {
    /// Parse the wire/CLI spelling. Case-insensitive.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "accurate" => Some(Self::Accurate),
            "efficient" => Some(Self::Efficient),
            _ => None,
        }
    }

    /// Must match the serde spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accurate => "accurate",
            Self::Efficient => "efficient",
        }
    }
}

/// What the caller wants from a backend for one loaded source
///
#[derive(Clone, Debug)]
pub struct PlaybackOptions {
    pub fit: Fit,
    pub looping: bool,
    pub max_fps: Option<u32>,
    pub power_saver: bool,
    /// Engine route to build at load. Carried
    /// over the existing win → media boundary so no trait method and no
    /// dependency edge had to be added.
    pub engine_mode: PresentationMode,
    /// Audio level 0–100 for this load: `0` = muted
    /// (default), `1–100` = audible at `level / 100`. Same boundary trick
    /// as `engine_mode`; runtime changes travel as `MediaBackend::set_volume`.
    pub volume: u8,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        // defaults: contain-only, loop on, uncapped fps,
        // no power profiles; the engine route defaults to honest colour.
        Self {
            fit: Fit::Contain,
            looping: true,
            max_fps: None,
            power_saver: false,
            engine_mode: PresentationMode::Accurate,
            volume: 0,
        }
    }
}

/// Backend counters. Surfaced to `status` in; measures externally
/// with GPU/CPU counters instead.
///
/// `hardware_decode` is optional because "cannot observe" and "observed
/// software decode" are different claims: the Media Engine reports neither,
/// so it answers `None` until the MFT probe exists and `status` omits
/// `hwdec` rather than asserting a `false` that would be wrong (measured
/// the GPU VideoDecode engine busy on this very build).
#[derive(Clone, Debug, Default)]
pub struct BackendStats {
    /// `None` = the backend cannot tell (omit the field on the wire).
    pub hardware_decode: Option<bool>,
    pub decoded_frames: u64,
    pub dropped_frames: u64,
    pub video_size: (u32, u32),
    pub fps: f32,
    /// Active presentation mode of the engine backend — `Some("frame-server")`
    /// (NV12 + our video processor) or `Some("rendering")` (the
    /// `MF_MEDIA_ENGINE_PLAYBACK_HWND` fallback, wrong colour on BT.601
    /// content by design). `None` while no
    /// playback thread runs, so `status` omits the field instead of
    /// guessing.
    pub engine_mode: Option<String>,
}

/// The only win → media boundary. Core stays Win32-free: the
/// HWND travels as a plain integer.
#[derive(Clone, Debug)]
pub struct HostSurface {
    pub hwnd: isize,
    pub monitor: MonitorId,
    /// Virtual-desktop rect of that monitor.
    pub rect: Rect,
    pub dpi: u32,
}

/// Errors a media backend can report.
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
/// depends on `cascade-win`.
pub trait MediaBackend {
    fn load(&mut self, source: &Path, opts: &PlaybackOptions) -> BackendResult<()>;
    fn play(&mut self) -> BackendResult<()>;
    fn pause(&mut self) -> BackendResult<()>;
    fn set_looping(&mut self, on: bool) -> BackendResult<()>;
    /// Apply an audio level 0–100 to the engine —
    /// a live property change like `play`/`pause`, never a reload. Backends
    /// with no audio session implement it as a no-op.
    fn set_volume(&mut self, level: u8) -> BackendResult<()>;
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
        // changing one is a breaking protocol change.
        assert_eq!(PauseReason::Fullscreen.as_str(), "fullscreen");
        assert_eq!(PauseReason::Maximized.as_str(), "maximized");
        assert_eq!(PauseReason::DisplayOff.as_str(), "display_off");
        assert_eq!(PauseReason::SessionLocked.as_str(), "session_locked");
        assert_eq!(PauseReason::Screensaver.as_str(), "screensaver");
        assert_eq!(PauseReason::RemoteSession.as_str(), "remote_session");
        assert_eq!(PauseReason::OnBattery.as_str(), "on_battery");
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
        // Bumping this without a update breaks old clients.
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn playback_defaults_match_m2_decisions() {
        let opts = PlaybackOptions::default();
        assert_eq!(opts.fit, Fit::Contain);
        assert!(opts.looping);
        assert_eq!(opts.max_fps, None);
        assert!(!opts.power_saver);
        // Honest colour is the default route.
        assert_eq!(opts.engine_mode, PresentationMode::Accurate);
        // Silent by default.
        assert_eq!(opts.volume, 0);
    }

    #[test]
    fn engine_mode_parses_and_serialises_as_documented() {
        // CLI/wire spelling: case-insensitive in, exact out.
        assert_eq!(
            PresentationMode::parse("accurate"),
            Some(PresentationMode::Accurate)
        );
        assert_eq!(
            PresentationMode::parse(" Efficient "),
            Some(PresentationMode::Efficient)
        );
        assert_eq!(PresentationMode::parse("fast"), None);
        assert_eq!(PresentationMode::Accurate.as_str(), "accurate");
        assert_eq!(PresentationMode::Efficient.as_str(), "efficient");
        assert_eq!(
            serde_json::to_value(PresentationMode::Efficient).expect("serialise"),
            serde_json::json!("efficient")
        );
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
            fn set_volume(&mut self, _level: u8) -> BackendResult<()> {
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
