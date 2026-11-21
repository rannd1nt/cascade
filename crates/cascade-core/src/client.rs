//! Named-pipe **client** transport for the control protocol
//! (`docs/ipc.md` §1 and §7).
//!
//! Moved here from `cascade-win` (design-m5 D10): `cascade-ui` must be
//! able to reach the daemon **without** depending on `cascade-win`
//! (AGENTS §4; `architecture.md` §3 — `cascade-ui ──▶ core, media,
//! never win`). The pipe **server** stays in `cascade-win`: it owns the
//! pipe thread, the `WM_APP_PIPE` wakeup and the snapshot channel —
//! daemon plumbing the UI never needs.
//!
//! One request is one JSON line out and one JSON line in: bounded connect
//! retries (2 s budget) and a 2 s response deadline, so a wedged daemon
//! can never hang a client forever (`ipc.md` §7).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use crate::ipc::{self, encode_line, no_data, DecodeError, MAX_LINE_BYTES, PIPE_PREFIX};
use crate::PROTOCOL_VERSION;

/// Client-side bounds (`ipc.md` §7): 2 s connect budget with retries, 2 s
/// for the response line.
const CONNECT_ATTEMPTS: u32 = 3;
const CONNECT_PAUSE: Duration = Duration::from_millis(250);
const RESPONSE_DEADLINE: Duration = Duration::from_secs(2);
const PEEK_POLL: Duration = Duration::from_millis(25);

/// `\\.\pipe\cascade-ctl-{session_id}` (`ipc.md` §1). Machine-global
/// namespace → branded prefix (design-m4 §2.5). Shared by the client and
/// the server (which lives in `cascade-win`), so both sides always agree
/// on the name.
pub fn server_name() -> String {
    let mut session: u32 = 0;
    // SAFETY: `GetCurrentProcessId` needs no arguments; `session` is a
    // valid local out-parameter the OS fills in.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    format!("{PIPE_PREFIX}{session}")
}

/// Why a CLI/GUI request failed. Messages are English and say what to do
/// next (`ipc.md` §8).
#[derive(Debug)]
pub enum ClientError {
    /// No pipe exists for this session: the daemon is not running.
    NotRunning,
    /// All instances busy after retries.
    Busy,
    /// Connected but no response line within the deadline.
    Timeout,
    /// Protocol versions differ; the user must restart Cascade.
    VersionMismatch,
    /// The daemon answered with an error envelope.
    Wire(crate::ipc::WireError),
    /// A Win32 failure outside the mapped cases.
    Io(u32),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => write!(
                f,
                "engine not running — start it with `cascade run` or enable autostart (`cascade autostart on`)"
            ),
            Self::Busy => write!(
                f,
                "the engine is busy with another request — retry in a moment"
            ),
            Self::Timeout => write!(
                f,
                "the engine accepted the request but did not respond within 2 s — retry, or check `cascade doctor`"
            ),
            Self::VersionMismatch => write!(
                f,
                "engine and app versions differ — restart Cascade"
            ),
            Self::Wire(err) => write!(f, "{}", err.message),
            Self::Io(code) => write!(f, "could not reach the engine (Win32 error {code})"),
        }
    }
}

/// Request id: unique per request **and** per process. The pipe thread
/// matches responses by id and drops stale ones, so two CLI invocations
/// must never collide on the same number (design-m4 §2.2).
fn next_request_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let slot = NEXT.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `GetCurrentProcessId` needs no arguments.
    let pid = unsafe { GetCurrentProcessId() } as u64;
    // High 32 bits identify the process, low 32 the request inside it.
    (pid << 32) | (slot & 0xFFFF_FFFF)
}

/// Send one command and return its `data` payload (`ipc.md` §7: bounded
/// connect retries, one line each way).
pub fn request(cmd: &str, data: serde_json::Value) -> Result<serde_json::Value, ClientError> {
    let name = to_wide(&server_name());
    let handle = connect(&name, GENERIC_READ | GENERIC_WRITE)?;

    let envelope = ipc::Request {
        v: PROTOCOL_VERSION,
        id: next_request_id(),
        cmd: cmd.to_string(),
        data,
    };
    let mut line = encode_line(&envelope).map_err(|_| ClientError::Io(0))?;
    line.push('\n');
    if !write_all(handle, line.as_bytes()) {
        // Capture before CloseHandle, which may overwrite the last error.
        // SAFETY: handle came from CreateFileW; closing once.
        let code = unsafe { GetLastError() };
        unsafe { CloseHandle(handle) };
        return Err(ClientError::Io(code));
    }

    let result = read_response_line(handle).and_then(|response| match response.v {
        v if v != PROTOCOL_VERSION => Err(ClientError::VersionMismatch),
        _ if response.id != envelope.id => Err(ClientError::Io(0)),
        _ if response.ok => Ok(response.data.unwrap_or_else(no_data)),
        _ => Err(match response.error {
            Some(err) => ClientError::Wire(err),
            None => ClientError::Io(0),
        }),
    });
    // SAFETY: handle came from CreateFileW; closed exactly once here.
    unsafe { CloseHandle(handle) };
    result
}

/// `\\.\pipe\cascade-evt-{session_id}` (`ipc.md` §1, design-m5-p4 §3).
/// Same session suffix as [`server_name`] so both sides always agree.
pub fn event_server_name() -> String {
    let mut session: u32 = 0;
    // SAFETY: `GetCurrentProcessId` needs no arguments; `session` is a
    // valid local out-parameter the OS fills in.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    format!("{}{session}", crate::ipc::EVENT_PIPE_PREFIX)
}

/// A held-open subscription to the daemon's event stream (`ipc.md` §6).
///
/// Server→client only: the client never writes, so the handle is opened
/// with `GENERIC_READ` against the server's outbound instance. One
/// request is one JSON line in (`ipc.md` §2); unlike [`request`], there
/// is **no response deadline** — the stream is read by a dedicated
/// thread that may legitimately wait hours for the next event, and a
/// dead daemon shows up as a read error the caller reconnects from
/// (design-m5-p4 §5).
pub struct EventStream {
    handle: HANDLE,
}

impl EventStream {
    /// Connect to the event pipe with the same bounded connect budget as
    /// [`request`] (2 s total, `ipc.md` §7).
    pub fn open() -> Result<Self, ClientError> {
        let name = to_wide(&event_server_name());
        let handle = connect(&name, GENERIC_READ)?;
        Ok(Self { handle })
    }

    /// Block until the next event line arrives. Errors mean the stream
    /// is over (daemon exited, pipe broken, protocol disagreement) —
    /// the caller reconnects and resyncs statelessly (`ipc.md` §7).
    pub fn read_event(&mut self) -> Result<ipc::Event, ClientError> {
        let mut pending: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(pos) = pending.iter().position(|&byte| byte == b'\n') {
                let line = String::from_utf8_lossy(&pending[..pos]).into_owned();
                return decode_event_line(&line);
            }
            if pending.len() > MAX_LINE_BYTES {
                return Err(ClientError::Io(0));
            }
            let mut read: u32 = 0;
            // SAFETY: `buf` is writable; null OVERLAPPED = synchronous
            // read, which blocks until at least one byte arrives.
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                // Capture before any later call can overwrite it.
                // SAFETY: reads the thread's last-error value.
                let code = unsafe { GetLastError() };
                return Err(ClientError::Io(code));
            }
            pending.extend_from_slice(&buf[..read as usize]);
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        // SAFETY: handle came from CreateFileW; closed exactly once.
        unsafe { CloseHandle(self.handle) };
    }
}

fn decode_event_line(line: &str) -> Result<ipc::Event, ClientError> {
    let event: ipc::Event = serde_json::from_str(line).map_err(|_| ClientError::Io(0))?;
    if event.v != PROTOCOL_VERSION {
        return Err(ClientError::VersionMismatch);
    }
    Ok(event)
}

fn connect(name: &[u16], access: u32) -> Result<HANDLE, ClientError> {
    let mut last_err: u32 = 0;
    for attempt in 0..CONNECT_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(CONNECT_PAUSE);
        }
        // SAFETY: `name` is NUL-terminated; null security/attrs use the
        // defaults; OPEN_EXISTING fails rather than creating anything.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(handle);
        }
        // SAFETY: reads the thread's last-error value.
        last_err = unsafe { GetLastError() };
        match last_err {
            // No daemon (yet): still worth the bounded retries so a `status`
            // fired right after start does not lose the race.
            code if code == ERROR_FILE_NOT_FOUND => continue,
            231 /* ERROR_PIPE_BUSY */ => continue,
            _ => break,
        }
    }
    Err(match last_err {
        // Retried out with no pipe at all.
        ERROR_FILE_NOT_FOUND | 0 => ClientError::NotRunning,
        231 => ClientError::Busy,
        other => ClientError::Io(other),
    })
}

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let mut written: u32 = 0;
        // SAFETY: valid buffer; null OVERLAPPED = synchronous write.
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            return false;
        }
        bytes = &bytes[written as usize..];
    }
    true
}

/// Read one newline-terminated response with a deadline: poll the pipe's
/// byte count, then read. A wedged daemon must not hang the CLI forever.
fn read_response_line(handle: HANDLE) -> Result<ipc::Response, ClientError> {
    let deadline = Instant::now() + RESPONSE_DEADLINE;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut available: u32 = 0;
        // SAFETY: null buffer with size 0 asks only for the byte count;
        // the out-params are valid locals.
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && available > 0 {
            let to_read = (available as usize).min(buf.len());
            let mut read: u32 = 0;
            // SAFETY: `buf` is writable for `to_read` bytes.
            let ok = unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr(),
                    to_read as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                return Err(ClientError::Io(unsafe { GetLastError() }));
            }
            pending.extend_from_slice(&buf[..read as usize]);
            if let Some(pos) = pending.iter().position(|&byte| byte == b'\n') {
                let line = String::from_utf8_lossy(&pending[..pos]).into_owned();
                return decode_response_line(&line);
            }
            if pending.len() > MAX_LINE_BYTES {
                return Err(ClientError::Io(0));
            }
        }
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout);
        }
        std::thread::sleep(PEEK_POLL);
    }
}

fn decode_response_line(line: &str) -> Result<ipc::Response, ClientError> {
    ipc::decode_response(line).map_err(|err| match err {
        DecodeError::VersionMismatch => ClientError::VersionMismatch,
        DecodeError::TooLarge => ClientError::Io(0),
        DecodeError::BadRequest(_) => ClientError::Io(0),
    })
}

/// Encode a Rust string as a NUL-terminated UTF-16 buffer — the form
/// `CreateFileW` expects. A private copy of `cascade_win::wide::to_wide`:
/// `cascade-core` must not expose a Win32 helper module for one call site
/// (design-m5 D10).
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_carries_the_session_suffix() {
        let name = server_name();
        assert!(name.starts_with(PIPE_PREFIX), "{name}");
        assert!(
            name[PIPE_PREFIX.len()..]
                .chars()
                .all(|c| c.is_ascii_digit()),
            "session id must be numeric: {name}"
        );
    }

    #[test]
    fn event_server_name_shares_the_session_but_not_the_namespace() {
        let name = event_server_name();
        assert!(
            name.starts_with(crate::ipc::EVENT_PIPE_PREFIX),
            "event pipe prefix expected: {name}"
        );
        let suffix = &name[crate::ipc::EVENT_PIPE_PREFIX.len()..];
        assert!(
            suffix.chars().all(|c| c.is_ascii_digit()),
            "session id must be numeric: {name}"
        );
        // Same session as the control pipe: both names are built from the
        // same ProcessIdToSessionId call.
        assert_eq!(
            suffix,
            &server_name()[PIPE_PREFIX.len()..],
            "event and control pipe must address the same session"
        );
    }

    #[test]
    fn event_lines_decode_only_as_events_with_our_version() {
        let good = r#"{"v":1,"event":"state","data":{"mode":"attached"}}"#;
        let event = decode_event_line(good).expect("valid event line");
        assert_eq!(event.event, "state");

        // A response envelope is not an event (no `event` field).
        let response = r#"{"v":1,"id":7,"ok":true,"data":null,"flush_after":false}"#;
        assert!(
            decode_event_line(response).is_err(),
            "responses are not events"
        );

        // Wrong version → the documented restart remedy, not a guess.
        let stale = r#"{"v":999,"event":"shutdown","data":{}}"#;
        assert!(matches!(
            decode_event_line(stale),
            Err(ClientError::VersionMismatch)
        ));
    }

    #[test]
    fn client_errors_say_what_to_do() {
        for err in [
            ClientError::NotRunning,
            ClientError::Busy,
            ClientError::Timeout,
            ClientError::VersionMismatch,
            ClientError::Io(5),
        ] {
            let text = err.to_string();
            assert!(!text.is_empty(), "every error needs a message");
            assert!(
                text.contains('—') || text.contains("error"),
                "actionable message expected: {text}"
            );
        }
    }

    #[test]
    fn request_ids_are_unique_per_process() {
        // Two CLI invocations in different processes must never collide:
        // the high bits carry the pid.
        let first = next_request_id();
        let second = next_request_id();
        assert_ne!(first, second, "ids must not repeat in one process");
        assert_eq!(first >> 32, second >> 32, "both ids belong to this process");
        assert_ne!(first >> 32, 0, "pid bits are populated");
    }

    #[test]
    fn to_wide_appends_nul() {
        assert_eq!(to_wide("abc"), vec![0x61, 0x62, 0x63, 0]);
        assert_eq!(to_wide(""), vec![0]);
    }
}
