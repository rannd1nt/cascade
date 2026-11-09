//! IPC protocol types and JSONL framing (`docs/ipc.md`).
//!
//! Pure data plus encode/decode: no Win32, no I/O, no threads — the wire is
//! newline-delimited JSON, UTF-8, one message per line (`ipc.md` §1–2).
//! The daemon-side pipe server and the CLI client live in `cascade-win`;
//! this module is what both sides (and the future GUI) agree on.
//!
//! Naming (design-m4 §2.5): the pipe name is a machine-global namespace, so
//! it carries the brand (`PIPE_PREFIX`); protocol constants and error-code
//! strings are our own vocabulary — subsystem prefix, no brand, and in JSON
//! the field itself is the namespace.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rule_engine::{BatteryAction, PauseSettings, PowerProfile};
use crate::PROTOCOL_VERSION;

/// Hard cap per line in bytes (`ipc.md` §2: 1 MiB, larger → `too_large`).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Named-pipe prefix; the session id is appended
/// (`ProcessIdToSessionId`, `ipc.md` §1).
pub const PIPE_PREFIX: &str = r"\\.\pipe\cascade-ctl-";

/// Error codes (`ipc.md` §8). Serialized as their wire strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    #[serde(rename = "bad_request")]
    BadRequest,
    #[serde(rename = "unknown_command")]
    UnknownCommand,
    #[serde(rename = "not_found")]
    NotFound,
    #[serde(rename = "unsupported_codec")]
    UnsupportedCodec,
    #[serde(rename = "file_missing")]
    FileMissing,
    #[serde(rename = "busy")]
    Busy,
    #[serde(rename = "internal")]
    Internal,
    #[serde(rename = "too_large")]
    TooLarge,
    #[serde(rename = "version_mismatch")]
    VersionMismatch,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::UnknownCommand => "unknown_command",
            Self::NotFound => "not_found",
            Self::UnsupportedCodec => "unsupported_codec",
            Self::FileMissing => "file_missing",
            Self::Busy => "busy",
            Self::Internal => "internal",
            Self::TooLarge => "too_large",
            Self::VersionMismatch => "version_mismatch",
        }
    }
}

/// Error payload of a failed response (`ipc.md` §2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    /// Human-readable, English, says what to do next where possible.
    pub message: String,
}

/// Request envelope (`ipc.md` §2).
///
/// Unknown fields are ignored by serde, which is the forward-compatibility
/// rule of the protocol: a newer client may add fields an older daemon
/// drops on the floor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub data: Value,
}

/// Response envelope (`ipc.md` §2). Exactly one of `data`/`error` is set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
    /// In-process handshake flag: the daemon set a different-file
    /// `wallpaper.set` and will restart itself right after the client has
    /// this answer (design-m2 §4). Never on the wire — it is daemon-side
    /// bookkeeping, so `ipc.md` §2 is unchanged.
    #[serde(skip)]
    pub flush_after: bool,
}

impl Response {
    pub fn ok(id: u64, data: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            data: Some(data),
            error: None,
            flush_after: false,
        }
    }

    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            data: None,
            error: Some(WireError {
                code,
                message: message.into(),
            }),
            flush_after: false,
        }
    }
}

/// Server-initiated event (`ipc.md` §6). No `id`, may arrive at any time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub event: String,
    pub data: Value,
}

/// Why a line could not be turned into a [`Request`], with the code the
/// server must report (`ipc.md` §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Line longer than [`MAX_LINE_BYTES`] (measured in bytes, not chars).
    TooLarge,
    /// Not valid JSON, or not a valid envelope.
    BadRequest(String),
    /// `v` is not [`PROTOCOL_VERSION`]; the client must tell the user to
    /// restart Cascade rather than guess (`ipc.md` §2).
    VersionMismatch,
}

impl DecodeError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::TooLarge => ErrorCode::TooLarge,
            Self::BadRequest(_) => ErrorCode::BadRequest,
            Self::VersionMismatch => ErrorCode::VersionMismatch,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::TooLarge => format!(
                "message exceeds {MAX_LINE_BYTES} bytes; send one JSON object per line"
            ),
            Self::BadRequest(detail) => format!("not a valid request envelope: {detail}"),
            Self::VersionMismatch => format!(
                "engine and app versions differ (protocol v{PROTOCOL_VERSION} expected) — restart Cascade"
            ),
        }
    }
}

/// Decode one request line (content without the trailing newline).
pub fn decode_request(line: &str) -> Result<Request, DecodeError> {
    // Byte length is the wire cap; `len()` on `&str` is bytes.
    if line.len() > MAX_LINE_BYTES {
        return Err(DecodeError::TooLarge);
    }
    let request: Request =
        serde_json::from_str(line).map_err(|err| DecodeError::BadRequest(err.to_string()))?;
    if request.v != PROTOCOL_VERSION {
        return Err(DecodeError::VersionMismatch);
    }
    Ok(request)
}

/// Decode one response line (client side). Version checking is the
/// caller's job so it can map a mismatch to `version_mismatch`.
pub fn decode_response(line: &str) -> Result<Response, DecodeError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(DecodeError::TooLarge);
    }
    serde_json::from_str(line).map_err(|err| DecodeError::BadRequest(err.to_string()))
}

/// Encode any envelope as one wire line (without the newline).
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, ErrorCode> {
    serde_json::to_string(value).map_err(|_| ErrorCode::Internal)
}

/// An empty `data` object — what requests without parameters carry.
pub fn no_data() -> Value {
    Value::Object(serde_json::Map::new())
}

/// `ping` payload (`ipc.md` §3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ping {
    pub pong: bool,
    pub version: u32,
    pub uptime_ms: u64,
}

// --- Command data shapes (ipc.md §3) -----------------------------------------

/// `autostart.get` / `autostart.set` request data. `autostart.get` ignores
/// it; `autostart.set` requires it (`{"enabled":true}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnabledData {
    pub enabled: bool,
}

/// `wallpaper.set` request data: exactly one of `path` (play this file
/// directly — S2) or `id` (library item — S3), plus an optional target
/// monitor. The server rejects `path` + `id` together and a payload with
/// neither (`bad_request`); an `id` alone answers `not_found` until the
/// library lands (design-m4 S3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

/// `wallpaper.clear` request data: an optional monitor. The engine is
/// single-surface in M4 (design-m2 §5), so the value is validated but the
/// one surface is cleared regardless.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

// --- Library shapes (ipc.md §3, design-m4 §4) --------------------------------

/// `library.add` request data: one or more file paths. The server
/// canonicalises, checks existence, probes metadata, and dedupes — the
/// client never sends metadata (single writer: the daemon main loop).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryAddData {
    pub paths: Vec<String>,
}

/// `library.remove` request data: the 4-hex entry id (`ipc.md` §3). The
/// CLI resolves a path argument to an id via `library.list` first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRemoveData {
    pub id: String,
}

/// `library.list` response: the full list, after any start-time prune.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryItemsData {
    pub items: Vec<crate::library::LibraryItem>,
}

/// A path `library.add` refused, with a reason the user can act on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedPath {
    pub path: String,
    pub reason: String,
}

/// `library.add` response: what got in and what did not (a rejected path
/// never leaves the desktop or the file system touched — nothing is copied).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryAddResult {
    pub added: Vec<crate::library::LibraryItem>,
    pub rejected: Vec<RejectedPath>,
    /// Per-added-entry assessment (S5, design-m4 §5.3 step 5). Omitted
    /// while empty so pre-S5 payloads stay byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assessments: Vec<ItemAssessment>,
}

/// `library.prune` response: how many entries were dropped because their
/// file no longer exists (design-m4 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryPruneResult {
    pub removed: u32,
}

// --- Assessment shapes (ipc.md §3, design-m4 §5, S5) ------------------------

/// `assess` request data: an optional file and an optional target monitor.
/// Neither = device profile + recommendation for the primary monitor
/// (design-m4 §3). Read-only; works while nothing is playing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

/// One monitor row of an `assess` response — the same geometry `status`
/// prints, plus the refresh/HDR facts the metrics used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorSummary {
    pub id: String,
    pub width: i32,
    pub height: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<bool>,
    pub primary: bool,
}

/// The device facts an `assess` response carries: what the CLI prints
/// with no file, and what `--json` scripts and the GUI read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceSummary {
    /// Sorted decode labels from the D12 union (D3D11 GPU profiles ∪
    /// hardware MFT input types), e.g. `["av01","h264","hevc", …]`.
    pub gpu_decode: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_total_mb: Option<u64>,
    pub monitors: Vec<MonitorSummary>,
}

/// `assess` response (`ipc.md` §3): the selected monitor, the device
/// facts, the recommendation for that monitor, and — when a file was
/// named — its descriptor plus the structured warnings. `warnings` is
/// always present (empty without a file); an `error` severity here is a
/// diagnosis, not a refusal (`assess` never blocks — only `set` does).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssessReport {
    pub monitor: String,
    pub device: DeviceSummary,
    pub recommendation: crate::assess::Recommendation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<crate::library::VideoMetadata>,
    #[serde(default)]
    pub warnings: Vec<crate::assess::Warning>,
}

/// One added library entry's assessment (design-m4 §5.3 step 5): the badge
/// data the M5 GUI renders per item, computed at the moment it enters the
/// library. Additive field on `library.add` — old clients ignore it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemAssessment {
    pub id: String,
    pub path: String,
    pub warnings: Vec<crate::assess::Warning>,
    pub recommendation: crate::assess::Recommendation,
}

/// `wallpaper.set` success data (additive, design-m4 §5.3): the assessment
/// that ran **before** the answer went out — `error` severities never
/// appear here (they were rejected instead). `recommendation` is `None`
/// only when the device reported no monitor to recommend for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<crate::library::VideoMetadata>,
    #[serde(default)]
    pub warnings: Vec<crate::assess::Warning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<crate::assess::Recommendation>,
}

// --- Status shape (ipc.md §4) ------------------------------------------------

/// Per-monitor row of [`Status`]. Fields that only exist once the library
/// and the engine report them (M4 S3+) are omitted while unset, so the
/// shape stays stable for clients.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorStatus {
    pub id: String,
    pub rect: crate::Rect,
    pub primary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<LibraryRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hwdec: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoSize>,
}

/// Library entry a monitor is playing (`ipc.md` §4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRef {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSize {
    pub w: u32,
    pub h: u32,
}

/// The daemon's own process counters — filled by the daemon, never guessed
/// by the client (`ipc.md` §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub working_set_mb: u64,
    pub private_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    /// `workerw_child` | `layered_progman_child` (diagnostic gold when
    /// debugging "desktop went blank", `ipc.md` §4).
    pub strategy: String,
    pub repaint_level: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartStatus {
    pub enabled: bool,
    pub windows_disabled: bool,
}

/// Full `status` payload (`ipc.md` §4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub engine: String,
    /// `boot | attached | paused | degraded | safe_mode | stopped`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paused_reasons: Vec<String>,
    pub monitors: Vec<MonitorStatus>,
    pub memory: MemoryStatus,
    pub host: HostStatus,
    pub autostart: AutostartStatus,
}

// --- Settings shape (ipc.md §5) ---------------------------------------------

/// Full `settings.get` / `settings.set` response payload (`ipc.md §5`).
///
/// `autostart` is reported here for wire completeness but is **not** file
/// backed: the Run key is the single oracle (design-m4 D10) and the daemon
/// fills this field from the registry at the wire edge. `settings.set` does
/// not carry it — that is `autostart.set`'s job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// D8 default `off`: no profile, no power reads.
    pub power_profile: PowerProfile,
    pub pause: PauseSettings,
    /// D8 default `off`: the power collector stays dark until opted in.
    pub on_battery: BatteryAction,
    /// Stored from S2; the exit ladder always restores the desktop anyway
    /// (AGENTS §3.1 outranks the flag — no runtime consumer yet).
    pub restore_wallpaper_on_exit: bool,
    /// Stored from S2; consumed by the M5 tray.
    pub tray: bool,
    /// Filled from the Run key at the wire edge; never persisted to
    /// `config.toml` (design-m4 D10).
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            power_profile: PowerProfile::Off,
            pause: PauseSettings::default(),
            on_battery: BatteryAction::Off,
            restore_wallpaper_on_exit: true,
            tray: true,
            autostart: false,
        }
    }
}

/// Partial `[pause]` overrides inside a `settings.set` payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PausePatch {
    pub fullscreen: Option<bool>,
    pub maximized: Option<bool>,
    pub display_off: Option<bool>,
    pub session_lock: Option<bool>,
    pub screensaver: Option<bool>,
    pub remote_session: Option<bool>,
}

/// Partial `settings.set` payload (`ipc.md §3`): only the present keys
/// change. Unknown keys are dropped by serde — the forward-compatibility
/// rule — so the CLI validates keys strictly before sending
/// (design-m4 §3: unknown flag → error, not ignored).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SettingsPatch {
    pub power_profile: Option<PowerProfile>,
    pub pause: Option<PausePatch>,
    pub on_battery: Option<BatteryAction>,
    pub restore_wallpaper_on_exit: Option<bool>,
    pub tray: Option<bool>,
}

impl Settings {
    /// The D9 **preset**: while `on_battery` is still the `off` default
    /// ("not set"), a profile suggests its default action. Explicit user
    /// values always win, so this never overwrites a non-`off` choice.
    pub fn apply_profile_preset(&mut self) {
        if self.on_battery != BatteryAction::Off {
            return;
        }
        match self.power_profile {
            PowerProfile::Battery => self.on_battery = BatteryAction::Pause,
            PowerProfile::Quality => self.on_battery = BatteryAction::Continue,
            PowerProfile::Off | PowerProfile::Balanced => {}
        }
    }

    /// Apply a partial update, then let `power_profile` act as the D9
    /// preset (design-m4 §2.6): it fills `on_battery` only while that value
    /// is still `off` and this patch did not touch it.
    pub fn apply_patch(&mut self, patch: SettingsPatch) {
        if let Some(profile) = patch.power_profile {
            self.power_profile = profile;
        }
        if let Some(pause) = patch.pause {
            if let Some(value) = pause.fullscreen {
                self.pause.fullscreen = value;
            }
            if let Some(value) = pause.maximized {
                self.pause.maximized = value;
            }
            if let Some(value) = pause.display_off {
                self.pause.display_off = value;
            }
            if let Some(value) = pause.session_lock {
                self.pause.session_lock = value;
            }
            if let Some(value) = pause.screensaver {
                self.pause.screensaver = value;
            }
            if let Some(value) = pause.remote_session {
                self.pause.remote_session = value;
            }
        }
        if let Some(action) = patch.on_battery {
            self.on_battery = action;
        }
        if let Some(value) = patch.restore_wallpaper_on_exit {
            self.restore_wallpaper_on_exit = value;
        }
        if let Some(value) = patch.tray {
            self.tray = value;
        }

        // Preset fill (design-m4 §2.6): only when this patch changed the
        // profile and did not touch `on_battery` itself.
        if patch.power_profile.is_some() && patch.on_battery.is_none() {
            self.apply_profile_preset();
        }
    }

    /// A contradictory-but-allowed combination (design-m4 §2.6): the
    /// precedence table already makes it deterministic, so `settings set`
    /// applies the value and the caller prints this warning instead of
    /// blocking. `None` means no conflict.
    pub fn conflict_warning(&self) -> Option<String> {
        match (self.on_battery, self.power_profile) {
            (BatteryAction::Continue, PowerProfile::Battery) => Some(
                "on_battery=continue contradicts power_profile=battery: on battery the video will keep playing"
                    .to_string(),
            ),
            (BatteryAction::Pause | BatteryAction::Stop, PowerProfile::Quality) => Some(format!(
                "on_battery={} contradicts power_profile=quality: quality promises no power pausing, but the video will still pause on battery",
                self.on_battery.as_str()
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn request_roundtrips() {
        let request = Request {
            v: PROTOCOL_VERSION,
            id: 7,
            cmd: "library.list".to_string(),
            data: no_data(),
        };
        let line = encode_line(&request).expect("encode");
        assert!(!line.contains('\n'), "one message per line");
        let back = decode_request(&line).expect("decode");
        assert_eq!(back.v, 1);
        assert_eq!(back.id, 7);
        assert_eq!(back.cmd, "library.list");
    }

    #[test]
    fn decode_ignores_unknown_fields() {
        // Forward compatibility: a newer client adds fields, we drop them.
        let line = r#"{"v":1,"id":3,"cmd":"ping","data":{},"future_field":[1,2]}"#;
        let request = decode_request(line).expect("parses");
        assert_eq!(request.cmd, "ping");
    }

    #[test]
    fn decode_accepts_missing_data_field() {
        let line = r#"{"v":1,"id":1,"cmd":"ping"}"#;
        let request = decode_request(line).expect("parses");
        assert!(request.data.is_null());
    }

    #[test]
    fn decode_rejects_bad_json() {
        let err = decode_request("not json").expect_err("must fail");
        assert_eq!(err.code(), ErrorCode::BadRequest);
        assert!(err.message().contains("envelope"), "{}", err.message());
    }

    #[test]
    fn decode_rejects_version_mismatch() {
        let err = decode_request(r#"{"v":99,"id":1,"cmd":"ping"}"#).expect_err("must fail");
        assert_eq!(err.code(), ErrorCode::VersionMismatch);
        assert!(
            err.message().contains("restart Cascade"),
            "message must say what to do: {}",
            err.message()
        );
    }

    #[test]
    fn decode_rejects_oversized_line() {
        let big = format!(r#"{{"v":1,"id":1,"cmd":"{}"}}"#, "x".repeat(MAX_LINE_BYTES));
        assert!(big.len() > MAX_LINE_BYTES);
        let err = decode_request(&big).expect_err("must fail");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        // Just under the cap still parses (or fails as JSON, not as size).
        let ok_line = r#"{"v":1,"id":1,"cmd":"ping"}"#;
        assert!(decode_request(ok_line).is_ok());
    }

    #[test]
    fn response_ok_and_error_shapes() {
        let ok = Response::ok(7, no_data());
        let line = encode_line(&ok).expect("encode");
        assert!(line.contains(r#""ok":true"#), "{line}");
        assert!(!line.contains("error"), "ok responses carry no error");

        let err = Response::err(7, ErrorCode::NotFound, "no such item");
        let line = encode_line(&err).expect("encode");
        assert!(line.contains(r#""ok":false"#), "{line}");
        assert!(line.contains(r#""code":"not_found"#), "{line}");

        let back = decode_response(&line).expect("decode");
        assert!(!back.ok);
        assert_eq!(
            back.error.map(|e| e.code),
            Some(ErrorCode::NotFound),
            "error code roundtrips as its wire string"
        );
    }

    #[test]
    fn error_code_wire_strings_are_stable() {
        // Renaming these breaks every client (docs/ipc.md §8).
        assert_eq!(ErrorCode::BadRequest.as_str(), "bad_request");
        assert_eq!(ErrorCode::UnknownCommand.as_str(), "unknown_command");
        assert_eq!(ErrorCode::NotFound.as_str(), "not_found");
        assert_eq!(ErrorCode::UnsupportedCodec.as_str(), "unsupported_codec");
        assert_eq!(ErrorCode::FileMissing.as_str(), "file_missing");
        assert_eq!(ErrorCode::Busy.as_str(), "busy");
        assert_eq!(ErrorCode::Internal.as_str(), "internal");
        assert_eq!(ErrorCode::TooLarge.as_str(), "too_large");
        assert_eq!(ErrorCode::VersionMismatch.as_str(), "version_mismatch");
        let line = encode_line(&ErrorCode::TooLarge).expect("encode");
        assert_eq!(line, r#""too_large""#);
    }

    #[test]
    fn status_serializes_with_the_documented_keys() {
        let status = Status {
            engine: "running".to_string(),
            mode: "attached".to_string(),
            paused_reasons: vec!["session_lock".to_string()],
            monitors: vec![MonitorStatus {
                id: r"\\.\DISPLAY1".to_string(),
                rect: crate::Rect::new(0, 0, 1366, 768),
                primary: true,
                source: None,
                hwdec: None,
                fps: None,
                dropped: None,
                video: None,
            }],
            memory: MemoryStatus {
                working_set_mb: 141,
                private_mb: 174,
            },
            host: HostStatus {
                strategy: "workerw_child".to_string(),
                repaint_level: 1,
            },
            autostart: AutostartStatus {
                enabled: true,
                windows_disabled: false,
            },
        };
        let line = encode_line(&status).expect("encode");
        for key in [
            r#""engine":"running""#,
            r#""mode":"attached""#,
            r#""paused_reasons":["session_lock"]"#,
            r#""working_set_mb":141"#,
            r#""strategy":"workerw_child""#,
            r#""windows_disabled":false"#,
        ] {
            assert!(line.contains(key), "missing {key} in {line}");
        }
        // Monitors that have no source yet omit the optional fields.
        assert!(!line.contains("hwdec"), "{line}");

        let back: Status = serde_json::from_str(&line).expect("decode");
        assert_eq!(back, status, "Status roundtrips");
    }

    #[test]
    fn pipe_prefix_is_the_branded_global_namespace() {
        // Design-m4 §2.5: pipe names are machine-global → brand required.
        assert_eq!(PIPE_PREFIX, r"\\.\pipe\cascade-ctl-");
    }

    #[test]
    fn settings_defaults_match_d8_and_the_documented_shape() {
        let settings = Settings::default();
        assert_eq!(settings.power_profile, PowerProfile::Off, "D8");
        assert_eq!(settings.on_battery, BatteryAction::Off, "D8");
        assert!(settings.restore_wallpaper_on_exit);
        assert!(settings.tray);
        assert!(
            !settings.autostart,
            "filled from the registry at the wire edge"
        );
        // ipc.md §5 documented pause defaults.
        assert!(settings.pause.fullscreen);
        assert!(!settings.pause.maximized);
        assert!(settings.pause.session_lock);
    }

    #[test]
    fn settings_serializes_with_the_documented_keys() {
        let line = encode_line(&Settings::default()).expect("encode");
        for key in [
            r#""power_profile":"off""#,
            r#""fullscreen":true"#,
            r#""maximized":false"#,
            r#""display_off":true"#,
            r#""session_lock":true"#,
            r#""screensaver":true"#,
            r#""remote_session":true"#,
            r#""on_battery":"off""#,
            r#""restore_wallpaper_on_exit":true"#,
            r#""tray":true"#,
            r#""autostart":false"#,
        ] {
            assert!(line.contains(key), "missing {key} in {line}");
        }
        let back: Settings = serde_json::from_str(&line).expect("decode");
        assert_eq!(back, Settings::default(), "Settings roundtrips");
    }

    #[test]
    fn settings_patch_is_partial_and_ignores_unknown_keys() {
        // Only present keys change; absent ones stay untouched.
        let mut settings = Settings::default();
        let patch: SettingsPatch =
            serde_json::from_str(r#"{"pause":{"maximized":true},"tray":false}"#).expect("patch");
        settings.apply_patch(patch);
        assert!(settings.pause.maximized);
        assert!(!settings.tray);
        assert!(settings.pause.fullscreen, "untouched keys keep their value");
        assert_eq!(settings.on_battery, BatteryAction::Off);

        // Forward compatibility: unknown keys drop on the floor (ipc.md §2).
        let patch: SettingsPatch =
            serde_json::from_str(r#"{"future_knob":7,"restore_wallpaper_on_exit":false}"#)
                .expect("patch");
        settings.apply_patch(patch);
        assert!(!settings.restore_wallpaper_on_exit);
    }

    #[test]
    fn power_profile_preset_fills_unset_battery_action() {
        // D9: the profile is a preset that only fills `on_battery` while it
        // is still off.
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Battery),
            ..SettingsPatch::default()
        });
        assert_eq!(
            settings.on_battery,
            BatteryAction::Pause,
            "preset filled it"
        );

        // An explicit value in the same patch beats the preset.
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Battery),
            on_battery: Some(BatteryAction::Continue),
            ..SettingsPatch::default()
        });
        assert_eq!(settings.on_battery, BatteryAction::Continue);

        // A value set earlier is never overwritten by a later preset.
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            on_battery: Some(BatteryAction::Continue),
            ..SettingsPatch::default()
        });
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Battery),
            ..SettingsPatch::default()
        });
        assert_eq!(
            settings.on_battery,
            BatteryAction::Continue,
            "explicit wins"
        );

        // Quality suggests continue; balanced/off have no opinion.
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Quality),
            ..SettingsPatch::default()
        });
        assert_eq!(settings.on_battery, BatteryAction::Continue);
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Balanced),
            ..SettingsPatch::default()
        });
        assert_eq!(
            settings.on_battery,
            BatteryAction::Off,
            "balanced has no opinion"
        );
    }

    #[test]
    fn command_data_shapes_decode_with_defaults() {
        // `wallpaper.set` with a direct path (S2) — id/monitor optional.
        let set: SetData =
            serde_json::from_str(r#"{"path":"C:\\videos\\clip.mp4"}"#).expect("set data");
        assert_eq!(set.path.as_deref(), Some(r"C:\videos\clip.mp4"));
        assert!(set.id.is_none());
        assert!(set.monitor.is_none());
        // The combination rules are the server's job; decoding stays dumb.
        let both: SetData =
            serde_json::from_str(r#"{"path":"a.mp4","id":"ab12"}"#).expect("both fields");
        assert!(both.path.is_some() && both.id.is_some());

        // `wallpaper.clear` with no monitor clears everything.
        let clear: MonitorData = serde_json::from_str("{}").expect("clear data");
        assert!(clear.monitor.is_none());

        // `autostart.set` requires its one field.
        let enabled: EnabledData = serde_json::from_str(r#"{"enabled":true}"#).expect("enabled");
        assert!(enabled.enabled);
        assert!(serde_json::from_str::<EnabledData>("{}").is_err());

        // Roundtrip with the skip rules: absent options stay absent.
        let line = encode_line(&SetData {
            path: Some("a.mp4".to_string()),
            id: None,
            monitor: Some(r"\\.\DISPLAY1".to_string()),
        })
        .expect("encode");
        assert!(!line.contains("\"id\""), "absent option omitted: {line}");
        let back: SetData = serde_json::from_str(&line).expect("decode");
        assert_eq!(back.monitor.as_deref(), Some(r"\\.\DISPLAY1"));
    }

    #[test]
    fn library_data_shapes_decode() {
        // `library.add` carries paths only — metadata is the daemon's job.
        let add: LibraryAddData =
            serde_json::from_str(r#"{"paths":["C:\\a.mp4","C:\\b.mp4"]}"#).expect("add data");
        assert_eq!(add.paths.len(), 2);
        assert!(serde_json::from_str::<LibraryAddData>("{}").is_err());

        // `library.remove` needs an id.
        let remove: LibraryRemoveData = serde_json::from_str(r#"{"id":"a3f2"}"#).expect("remove");
        assert_eq!(remove.id, "a3f2");
        assert!(serde_json::from_str::<LibraryRemoveData>("{}").is_err());

        // Responses round-trip with the documented keys.
        let result = LibraryAddResult {
            added: Vec::new(),
            rejected: vec![RejectedPath {
                path: "C:\\gone.mp4".to_string(),
                reason: "file not found".to_string(),
            }],
            assessments: Vec::new(),
        };
        let line = encode_line(&result).expect("encode");
        assert!(
            line.contains("\"added\"") && line.contains("\"rejected\""),
            "{line}"
        );
        assert!(line.contains("file not found"), "{line}");
        // Empty additive fields stay off the wire (pre-S5 payloads are
        // byte-identical).
        assert!(!line.contains("assessments"), "{line}");

        let items: LibraryItemsData = serde_json::from_str(r#"{"items":[]}"#).expect("list data");
        assert!(items.items.is_empty());

        let pruned: LibraryPruneResult = serde_json::from_str(r#"{"removed":3}"#).expect("prune");
        assert_eq!(pruned.removed, 3);
    }

    /// S5 wire shapes (ipc.md §3): `assess` request/report round-trip,
    /// and the additive `set`/`add` fields tolerate pre-S5 payloads.
    #[test]
    fn assess_shapes_round_trip_and_stay_additive() {
        use crate::assess::Recommendation;

        // An empty object is a valid `assess` request (device profile).
        let empty: AssessRequest = serde_json::from_str("{}").expect("empty request");
        assert_eq!(empty, AssessRequest::default());
        let request: AssessRequest =
            serde_json::from_str(r#"{"path":"C:\\a.mp4","monitor":"\\\\.\\DISPLAY1"}"#)
                .expect("request");
        assert_eq!(request.path.as_deref(), Some(r"C:\a.mp4"));
        assert_eq!(request.monitor.as_deref(), Some(r"\\.\DISPLAY1"));

        let report = AssessReport {
            monitor: r"\\.\DISPLAY1".to_string(),
            device: DeviceSummary::default(),
            recommendation: Recommendation {
                width: 1366,
                height: 768,
                note: "exact display resolution".to_string(),
            },
            video: None,
            warnings: Vec::new(),
        };
        let line = encode_line(&report).expect("encode");
        assert!(
            line.contains("\"monitor\"") && line.contains("\"recommendation\""),
            "{line}"
        );
        assert!(line.contains("\"device\""), "{line}");
        // Optional keys stay off the wire while unset.
        assert!(!line.contains("\"video\""), "{line}");
        let back: AssessReport = serde_json::from_str(&line).expect("decode");
        assert_eq!(back, report);

        // A pre-S5 `wallpaper.set` success payload decodes with defaults —
        // an old daemon's `{}` never breaks the new CLI.
        let legacy: SetReport = serde_json::from_str("{}").expect("legacy set payload");
        assert!(legacy.warnings.is_empty());
        assert!(legacy.video.is_none());
        assert!(legacy.recommendation.is_none());
    }

    #[test]
    fn conflict_warning_flags_contradictory_combinations() {
        // §2.6 C1/C2: warn, but the precedence table keeps it deterministic.
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Battery),
            on_battery: Some(BatteryAction::Continue),
            ..SettingsPatch::default()
        });
        let warning = settings
            .conflict_warning()
            .expect("continue vs battery warns");
        assert!(warning.contains("on_battery=continue"), "{warning}");

        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Quality),
            on_battery: Some(BatteryAction::Pause),
            ..SettingsPatch::default()
        });
        let warning = settings.conflict_warning().expect("pause vs quality warns");
        assert!(warning.contains("power_profile=quality"), "{warning}");

        // Consistent and default combinations stay quiet.
        assert_eq!(Settings::default().conflict_warning(), None);
        let mut settings = Settings::default();
        settings.apply_patch(SettingsPatch {
            power_profile: Some(PowerProfile::Battery),
            ..SettingsPatch::default()
        });
        assert_eq!(
            settings.conflict_warning(),
            None,
            "the preset produces a consistent pair"
        );
    }
}
