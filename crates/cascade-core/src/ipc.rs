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
}

impl Response {
    pub fn ok(id: u64, data: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            data: Some(data),
            error: None,
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
}
