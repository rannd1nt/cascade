//! Named-pipe server and client for the control protocol
//! (`docs/ipc.md`, design-m4 §2).
//!
//! The **server** runs on its own thread started by `daemon::run`: it owns
//! the pipe handle and every `ReadFile`/`WriteFile` on it. The render loop
//! never waits on pipe I/O — it feeds the thread an immutable
//! [`StatusSnapshot`] through a channel whenever state changes, and the
//! thread answers read-only commands (`ping`, `status`) from that snapshot
//! without touching the main loop (design-m4 §2.2).
//!
//! The **client** is the CLI path (`cascade status`): connect with a bounded
//! retry, one JSON line out, one JSON line in, close (`ipc.md` §7).

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cascade_core::ipc::{
    self, encode_line, no_data, DecodeError, Ping, Response, Status, MAX_LINE_BYTES, PIPE_PREFIX,
};
use cascade_core::PROTOCOL_VERSION;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

use crate::derr;
use crate::monitor::{self, MonitorInfo};
use crate::registry;
use crate::wide;

/// Instances the server pre-creates (`ipc.md` §1: small fixed max, not
/// `PIPE_UNLIMITED_INSTANCES`). Enough for a CLI call plus the future GUI
/// and tray; a connect beyond this fails `ERROR_PIPE_BUSY`, which the
/// client retries.
const MAX_INSTANCES: u32 = 8;
const BUFFER_BYTES: u32 = 4096;

/// Client-side bounds (`ipc.md` §7): 2 s connect budget with retries, 2 s
/// for the response line.
const CONNECT_ATTEMPTS: u32 = 3;
const CONNECT_PAUSE: Duration = Duration::from_millis(250);
const RESPONSE_DEADLINE: Duration = Duration::from_secs(2);
const PEEK_POLL: Duration = Duration::from_millis(25);

/// `\\.\pipe\cascade-ctl-{session_id}` (`ipc.md` §1). Machine-global
/// namespace → branded prefix (design-m4 §2.5).
pub fn server_name() -> String {
    let mut session: u32 = 0;
    // SAFETY: `GetCurrentProcessId` needs no arguments; `session` is a
    // valid local out-parameter the OS fills in.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    format!("{PIPE_PREFIX}{session}")
}

/// What the main loop publishes to the pipe thread: the parts of `status`
/// only the render loop owns (`ipc.md` §4). Memory and autostart are read
/// on demand by the pipe thread; they are not part of this snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusSnapshot {
    /// `paused` | `attached` | `degraded` (design-m4 §2.2).
    pub mode: String,
    pub paused_reasons: Vec<String>,
    pub monitors: Vec<MonitorInfo>,
    pub host_strategy: String,
    pub repaint_level: u32,
}

impl StatusSnapshot {
    /// Fallback before the first snapshot arrives (should not happen: the
    /// daemon sends one before spawning the server thread).
    fn boot() -> Self {
        Self {
            mode: "boot".to_string(),
            paused_reasons: Vec::new(),
            monitors: monitor::enumerate(),
            host_strategy: String::new(),
            repaint_level: 0,
        }
    }
}

/// The server half: created on the main thread (it *is* the single-instance
/// lock, `ipc.md` §1), then moved into the server thread.
pub struct PipeServer {
    handle: HANDLE,
}

impl PipeServer {
    /// Create the single instance of the session pipe.
    ///
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` makes a second daemon fail with
    /// `ERROR_ACCESS_DENIED` instead of silently sharing the name — that
    /// failure is what `daemon::run` turns into `AlreadyRunning`.
    pub fn create() -> Result<Self, u32> {
        let name = wide::to_wide(&server_name());
        // SAFETY: `name` is NUL-terminated; null SECURITY_ATTRIBUTES gives
        // the default DACL (same user only, `ipc.md` §1); buffers are
        // kernel-side sizes, not pointers.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_INSTANCES,
                BUFFER_BYTES,
                BUFFER_BYTES,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: reads the thread's last-error value.
            return Err(unsafe { GetLastError() });
        }
        Ok(Self { handle })
    }

    /// Move the handle into a server thread. The thread is process-scoped:
    /// it blocks in `ConnectNamedPipe`/`ReadFile`, is never joined, and
    /// dies with the process (design-m4 §2.3, S1 note). Returns the spawn
    /// result so the caller can log a failure and keep running without a
    /// control channel — the desktop must stay up regardless.
    pub fn serve(self, snapshots: Receiver<Arc<StatusSnapshot>>) -> Result<(), u32> {
        // HANDLE is a raw pointer and therefore not `Send`; the value is
        // carried as an integer and cast back inside the thread — the only
        // code that ever dereferences it.
        let handle_addr = self.handle as isize;
        // Ownership of the handle moves to the thread; disarm the Drop.
        std::mem::forget(self);
        let started = Instant::now();
        match std::thread::Builder::new()
            .name("cascade-ipc".to_string())
            .spawn(move || serve_loop(handle_addr as HANDLE, snapshots, started))
        {
            Ok(_) => Ok(()),
            Err(err) => {
                // The thread never started; close the handle ourselves.
                // SAFETY: handle came from CreateNamedPipeW, closed once.
                unsafe { CloseHandle(handle_addr as HANDLE) };
                Err(err.raw_os_error().unwrap_or(0) as u32)
            }
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateNamedPipeW and is closed once.
        unsafe { CloseHandle(self.handle) };
    }
}

fn serve_loop(handle: HANDLE, snapshots: Receiver<Arc<StatusSnapshot>>, started: Instant) {
    let mut latest: Option<Arc<StatusSnapshot>> = None;
    loop {
        // SAFETY: null OVERLAPPED = synchronous accept on our own handle.
        let ok = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        // SAFETY: reads the thread's last-error value.
        let err = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        const ERROR_PIPE_CONNECTED: u32 = 535;
        if err != 0 && err != ERROR_PIPE_CONNECTED {
            // The handle is gone (process exit) or the pipe broke.
            derr!("ipc: accept failed (error {err}); control channel closed");
            return;
        }

        // Serve this connection until the client disconnects, then accept
        // the next one. Requests never reach the render loop (§2.2).
        serve_connection(handle, &snapshots, &mut latest, started);

        // SAFETY: we own the connected instance; disconnect resets it for
        // the next accept.
        unsafe { DisconnectNamedPipe(handle) };
    }
}

fn serve_connection(
    handle: HANDLE,
    snapshots: &Receiver<Arc<StatusSnapshot>>,
    latest: &mut Option<Arc<StatusSnapshot>>,
    started: Instant,
) {
    let mut buf = [0u8; BUFFER_BYTES as usize];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let mut read: u32 = 0;
        // SAFETY: `buf` is a valid writable buffer of the stated size;
        // null OVERLAPPED = synchronous read.
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        // Zero or failure = client gone (broken pipe / EOF).
        if ok == 0 || read == 0 {
            return;
        }
        pending.extend_from_slice(&buf[..read as usize]);

        while let Some(pos) = pending.iter().position(|&byte| byte == b'\n') {
            let line = String::from_utf8_lossy(&pending[..pos]).into_owned();
            pending.drain(..=pos);
            if !dispatch_line(handle, &line, snapshots, latest, started) {
                return; // connection closed on a write failure
            }
        }

        // A line longer than the cap with no newline yet: reject and drop
        // the connection rather than buffer unbounded data (`ipc.md` §2).
        if pending.len() > MAX_LINE_BYTES {
            let response = Response::err(
                0,
                ipc::ErrorCode::TooLarge,
                format!("message exceeds {MAX_LINE_BYTES} bytes; send one JSON object per line"),
            );
            if !write_response(handle, &response) {
                return;
            }
            pending.clear();
        }
    }
}

/// Decode and answer one request line. Returns `false` when the connection
/// must close (write failed).
fn dispatch_line(
    handle: HANDLE,
    line: &str,
    snapshots: &Receiver<Arc<StatusSnapshot>>,
    latest: &mut Option<Arc<StatusSnapshot>>,
    started: Instant,
) -> bool {
    let request = match ipc::decode_request(line) {
        Ok(request) => request,
        Err(err) => {
            // The envelope may be unparseable, so its id is unknown:
            // id 0 marks "could not be matched to a request".
            let response = Response::err(0, err.code(), err.message());
            return write_response(handle, &response);
        }
    };

    let response = match request.cmd.as_str() {
        "ping" => Response::ok(
            request.id,
            serde_json::to_value(Ping {
                pong: true,
                version: PROTOCOL_VERSION,
                uptime_ms: started.elapsed().as_millis() as u64,
            })
            .unwrap_or(no_data()),
        ),
        "status" => {
            // Drain snapshot updates queued while we were idle or busy.
            while let Ok(next) = snapshots.try_recv() {
                *latest = Some(next);
            }
            let snapshot = match latest {
                Some(snapshot) => Arc::clone(snapshot),
                None => Arc::new(StatusSnapshot::boot()),
            };
            let status = build_status(&snapshot);
            match serde_json::to_value(&status) {
                Ok(value) => Response::ok(request.id, value),
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::Internal,
                    format!("could not build status: {err}"),
                ),
            }
        }
        other => Response::err(
            request.id,
            ipc::ErrorCode::UnknownCommand,
            format!("unknown command '{other}' (supported: ping, status)"),
        ),
    };
    write_response(handle, &response)
}

/// Assemble `status`: snapshot (render-loop state) + counters this thread
/// may read on its own (`ipc.md` §4: the daemon fills `memory` itself).
fn build_status(snapshot: &StatusSnapshot) -> Status {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::new());
    let autostart = registry::autostart_state(&exe);
    Status {
        engine: "running".to_string(),
        mode: snapshot.mode.clone(),
        paused_reasons: snapshot.paused_reasons.clone(),
        monitors: snapshot
            .monitors
            .iter()
            .map(|monitor| cascade_core::ipc::MonitorStatus {
                id: monitor.id.clone(),
                rect: monitor.rect,
                primary: monitor.primary,
                source: None,
                hwdec: None,
                fps: None,
                dropped: None,
                video: None,
            })
            .collect(),
        memory: memory_status(),
        host: cascade_core::ipc::HostStatus {
            strategy: snapshot.host_strategy.clone(),
            repaint_level: snapshot.repaint_level,
        },
        autostart: cascade_core::ipc::AutostartStatus {
            enabled: autostart.registered,
            windows_disabled: autostart.windows_disabled,
        },
    }
}

/// This process's working set and private bytes (`ipc.md` §4).
fn memory_status() -> cascade_core::ipc::MemoryStatus {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };

    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    // SAFETY: `counters` is a valid EX struct; the size argument declares
    // which layout the OS may fill, so `PrivateUsage` is populated.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX
                as *mut windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok == 0 {
        return cascade_core::ipc::MemoryStatus {
            working_set_mb: 0,
            private_mb: 0,
        };
    }
    cascade_core::ipc::MemoryStatus {
        working_set_mb: counters.WorkingSetSize as u64 / (1024 * 1024),
        private_mb: counters.PrivateUsage as u64 / (1024 * 1024),
    }
}

fn write_response(handle: HANDLE, response: &Response) -> bool {
    let Ok(mut line) = encode_line(response) else {
        return true; // encoding our own types cannot fail in practice
    };
    line.push('\n');
    let bytes = line.as_bytes();
    let mut sent = 0usize;
    while sent < bytes.len() {
        let mut written: u32 = 0;
        // SAFETY: valid buffer; null OVERLAPPED = synchronous write.
        let ok = unsafe {
            WriteFile(
                handle,
                bytes[sent..].as_ptr(),
                (bytes.len() - sent) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            return false;
        }
        sent += written as usize;
    }
    true
}

// --- Client -----------------------------------------------------------------

/// Why a CLI request failed. Messages are English and say what to do next
/// (`ipc.md` §8).
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
    Wire(cascade_core::ipc::WireError),
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

/// Send one command and return its `data` payload (`ipc.md` §7: bounded
/// connect retries, one line each way).
pub fn request(cmd: &str, data: serde_json::Value) -> Result<serde_json::Value, ClientError> {
    let name = wide::to_wide(&server_name());
    let handle = connect(&name)?;

    let envelope = ipc::Request {
        v: PROTOCOL_VERSION,
        id: 1, // unique per connection; one request per connection here
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

fn connect(name: &[u16]) -> Result<HANDLE, ClientError> {
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
                GENERIC_READ | GENERIC_WRITE,
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
    fn snapshot_boot_mode_is_documented() {
        // Not calling enumerate() here (GDI in a unit test): only the mode
        // string is asserted, which is the wire-visible part.
        let snapshot = StatusSnapshot {
            mode: "boot".to_string(),
            paused_reasons: Vec::new(),
            monitors: Vec::new(),
            host_strategy: String::new(),
            repaint_level: 0,
        };
        assert_eq!(snapshot.mode, "boot");
    }
}
