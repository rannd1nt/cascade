//! Named-pipe **server** for the control protocol
//! (`docs/ipc.md`, design-m4 §2).
//!
//! The **server** runs on its own thread started by `daemon::run`: it owns
//! the pipe handle and every `ReadFile`/`WriteFile` on it. The render loop
//! never waits on pipe I/O. Read-only commands (`ping`, `status`,
//! `settings.get`, `autostart.*`) are answered on this thread from an
//! immutable snapshot channel fed by the main loop; mutating commands
//! (`playback.*`, `wallpaper.*`, `settings.set`, `quit`) are handed to the
//! main loop through a command channel, woken with `WM_APP_PIPE`, and the
//! thread blocks on the response for at most 2 s before answering `busy`
//! (design-m4 §2.2).
//!
//! The **client** lives in `cascade_core::client` (design-m5 D10 — the
//! UI and the CLI share one transport without depending on this crate):
//! connect with a bounded retry, one JSON line out, one JSON line in,
//! close (`ipc.md` §7).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cascade_core::client::server_name;
use cascade_core::ipc::{
    self, encode_line, no_data, AssessRequest, AutostartStatus, EnabledData, LibraryAddData,
    LibraryRef, LibraryRemoveData, MonitorData, Ping, Response, SetData, Settings, SettingsPatch,
    Status, VideoSize, MAX_LINE_BYTES,
};
use cascade_core::PROTOCOL_VERSION;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::control::{self, WM_APP_PIPE};
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

/// Server-side bounds (design-m4 §2.2): how long the pipe thread waits for
/// the main loop to answer a forwarded command, and how long the main loop
/// waits for the `quit` flush ack before quitting anyway (the ack itself
/// is sent by the pipe thread, so this constant is shared with `daemon`).
const SUBMIT_DEADLINE: Duration = Duration::from_secs(2);
pub const QUIT_FLUSH_WAIT: Duration = Duration::from_secs(1);

/// What the main loop publishes to the pipe thread: the parts of `status`
/// only the render loop owns (`ipc.md` §4), plus the settings the daemon
/// persists (design-m4 D10). Memory and autostart are read on demand by
/// the pipe thread; they are not part of this snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusSnapshot {
    /// `paused` | `attached` | `degraded` (design-m4 §2.2).
    pub mode: String,
    pub paused_reasons: Vec<String>,
    pub monitors: Vec<MonitorInfo>,
    /// Playback counters for the one monitor the engine draws on
    /// (M4 S3b); `None` while no video is loaded and attached.
    pub playback: Option<PlaybackStatus>,
    pub host_strategy: String,
    pub repaint_level: u32,
    /// File-backed settings; the `autostart` field inside is the
    /// placeholder — the wire edge fills it from the registry.
    pub settings: Settings,
}

/// The `monitors[]` row the engine is actually drawing on (`ipc.md` §4).
///
/// Built on the render loop from `MediaBackend::stats()` plus the applied
/// source. Every field is `Option` for an honest reason: `None` means the
/// daemon has nothing to report (no library entry, a backend that cannot
/// observe hardware decode, a counter the engine does not deliver) — never
/// a placeholder value (M4 S3b).
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackStatus {
    /// Monitor id of the host window the engine is attached to. The row
    /// with this id gets the fields; every other monitor stays `None`
    /// (single-surface engine, design-m2 §5).
    pub monitor: String,
    /// Library entry for the applied path, when the path is in the library.
    pub source: Option<LibraryRef>,
    pub hwdec: Option<bool>,
    pub fps: Option<f32>,
    pub dropped: Option<u64>,
    pub video: Option<VideoSize>,
    /// Media Engine mode (`"frame-server"` / `"rendering"`), when the
    /// backend reports one (`design-p1-color.md` §4.4).
    pub engine: Option<String>,
}

impl StatusSnapshot {
    /// Fallback before the first snapshot arrives (should not happen: the
    /// daemon sends one before spawning the server thread).
    fn boot() -> Self {
        Self {
            mode: "boot".to_string(),
            paused_reasons: Vec::new(),
            monitors: monitor::enumerate(),
            playback: None,
            host_strategy: String::new(),
            repaint_level: 0,
            settings: Settings::default(),
        }
    }
}

/// Main loop → pipe thread: a fresh snapshot, or the response to a
/// forwarded command (design-m4 §2.2). Both travel one channel so the
/// thread can absorb snapshot updates while it waits for a response.
pub enum ServerMsg {
    Snapshot(Arc<StatusSnapshot>),
    Response(Response),
}

/// Which role the pipe thread serves (`design-m5b.md` §4.5).
///
/// [`PipeRole::Daemon`] is today's behaviour byte for byte: `ping`/`status`
/// from the snapshot, mutating commands forwarded to the main loop, the
/// flush-ack handshake for `quit`. [`PipeRole::Listener`] answers in place —
/// the listener has no engine and no command loop, so only the commands it
/// can answer honestly are served (`design-m5b.md` §4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeRole {
    Daemon,
    Listener,
}

/// Mutating commands the pipe thread hands to the main loop. Each carries
/// its request id so the response can be matched — and stale responses
/// from a submit that timed out dropped — on the shared channel.
pub enum Command {
    Pause {
        id: u64,
    },
    Resume {
        id: u64,
    },
    SetWallpaper {
        id: u64,
        path: String,
        monitor: Option<String>,
    },
    ClearWallpaper {
        id: u64,
        monitor: Option<String>,
    },
    Restore {
        id: u64,
    },
    SettingsSet {
        id: u64,
        patch: SettingsPatch,
    },
    /// `library.list` — answer with the current items (post-prune).
    LibraryList {
        id: u64,
    },
    /// `library.add` — canonicalise, probe, dedupe, persist (design-m4 §4).
    LibraryAdd {
        id: u64,
        paths: Vec<String>,
    },
    /// `library.remove` — edit the list only (AGENTS §3.3).
    LibraryRemove {
        id: u64,
        item_id: String,
    },
    /// `library.prune` — drop entries whose file is gone.
    LibraryPrune {
        id: u64,
    },
    /// `wallpaper.set` with a library id: resolve it to the stored path,
    /// then run the same validated path flow as `SetWallpaper`.
    SetWallpaperById {
        id: u64,
        item_id: String,
        monitor: Option<String>,
    },
    /// `assess` (S5, design-m4 §5): probe-once when the library's
    /// `mtime`+`size` proof is stale, run the pure metric engine, answer
    /// with the report. Read-only — never touches playback.
    Assess {
        id: u64,
        path: Option<String>,
        monitor: Option<String>,
    },
    Quit {
        id: u64,
    },
}

impl Command {
    /// The request id this command answers.
    pub fn id(&self) -> u64 {
        match self {
            Self::Pause { id }
            | Self::Resume { id }
            | Self::SetWallpaper { id, .. }
            | Self::ClearWallpaper { id, .. }
            | Self::Restore { id }
            | Self::SettingsSet { id, .. }
            | Self::LibraryList { id }
            | Self::LibraryAdd { id, .. }
            | Self::LibraryRemove { id, .. }
            | Self::LibraryPrune { id }
            | Self::SetWallpaperById { id, .. }
            | Self::Assess { id, .. }
            | Self::Quit { id } => *id,
        }
    }
}

/// The channel half the server thread owns (design-m4 §2.2): it receives
/// snapshots and responses, forwards commands, and acks a response whose
/// write is done (`quit`, and a respawn-triggering `wallpaper.set` —
/// design-m2 §4). The main loop keeps the mirrored senders/receivers.
pub struct PipeChannels {
    pub messages: Receiver<ServerMsg>,
    pub commands: Sender<Command>,
    pub flush_acked: Sender<()>,
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
    /// dies with the process (design-m4 §2.3, S1 note). `role` selects the
    /// dispatch behaviour (`design-m5b.md` §4.5). Returns the spawn
    /// result so the caller can log a failure and keep running without a
    /// control channel — the desktop must stay up regardless.
    pub fn serve(self, channels: PipeChannels, role: PipeRole) -> Result<(), u32> {
        // HANDLE is a raw pointer and therefore not `Send`; the value is
        // carried as an integer and cast back inside the thread — the only
        // code that ever dereferences it.
        let handle_addr = self.handle as isize;
        // Ownership of the handle moves to the thread; disarm the Drop.
        std::mem::forget(self);
        let started = Instant::now();
        match std::thread::Builder::new()
            .name("cascade-ipc".to_string())
            .spawn(move || serve_loop(handle_addr as HANDLE, channels, started, role))
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

fn serve_loop(handle: HANDLE, channels: PipeChannels, started: Instant, role: PipeRole) {
    let mut latest: Option<Arc<StatusSnapshot>> = None;
    loop {
        // Keep the queue bounded while no client is connected: the newest
        // snapshot wins, stale responses are dropped.
        drain(&channels, &mut latest);

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
        // the next one.
        serve_connection(handle, &channels, &mut latest, started, role);

        // SAFETY: we own the connected instance; disconnect resets it for
        // the next accept.
        unsafe { DisconnectNamedPipe(handle) };
    }
}

fn serve_connection(
    handle: HANDLE,
    channels: &PipeChannels,
    latest: &mut Option<Arc<StatusSnapshot>>,
    started: Instant,
    role: PipeRole,
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
            if !dispatch_line(handle, &line, channels, latest, started, role) {
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
    channels: &PipeChannels,
    latest: &mut Option<Arc<StatusSnapshot>>,
    started: Instant,
    role: PipeRole,
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

    // The listener answers in place (`design-m5b.md` §4.5): no command loop
    // exists on the other side of its channel. Envelope errors above stay
    // shared — both roles see the same protocol edge.
    if role == PipeRole::Listener {
        return dispatch_listener(handle, &request, channels, latest, started);
    }

    let response = match request.cmd.as_str() {
        "ping" => Response::ok(
            request.id,
            serde_json::to_value(Ping {
                pong: true,
                version: PROTOCOL_VERSION,
                uptime_ms: started.elapsed().as_millis() as u64,
            })
            .unwrap_or_else(|_| no_data()),
        ),
        "status" => {
            // Drain snapshot updates queued while we were idle or busy.
            drain(channels, latest);
            let snapshot = snapshot_or_boot(latest);
            match serde_json::to_value(build_status(&snapshot)) {
                Ok(value) => Response::ok(request.id, value),
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::Internal,
                    format!("could not build status: {err}"),
                ),
            }
        }
        "settings.get" => {
            drain(channels, latest);
            let mut settings = snapshot_or_boot(latest).settings;
            // Wire edge (design-m4 D10): the Run key, never the file.
            settings.autostart = registry::autostart_state(&exe_path()).registered;
            match serde_json::to_value(settings) {
                Ok(value) => Response::ok(request.id, value),
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::Internal,
                    format!("could not build settings: {err}"),
                ),
            }
        }
        "autostart.get" => match serde_json::to_value(autostart_status()) {
            Ok(value) => Response::ok(request.id, value),
            Err(err) => Response::err(
                request.id,
                ipc::ErrorCode::Internal,
                format!("could not build autostart status: {err}"),
            ),
        },
        "autostart.set" => match serde_json::from_value::<EnabledData>(request.data.clone()) {
            Ok(data) => {
                // Registry work is cheap and the Run key is the oracle
                // (design-m4 D10): handled right here, no main loop.
                let result = if data.enabled {
                    registry::autostart_enable(&exe_path())
                } else {
                    registry::autostart_disable()
                };
                match result {
                    Ok(()) => match serde_json::to_value(autostart_status()) {
                        Ok(value) => Response::ok(request.id, value),
                        Err(err) => Response::err(
                            request.id,
                            ipc::ErrorCode::Internal,
                            format!("could not build autostart status: {err}"),
                        ),
                    },
                    Err(code) => Response::err(
                        request.id,
                        ipc::ErrorCode::Internal,
                        format!("could not update the Run key (Win32 error {code}); check Settings > Apps > Startup"),
                    ),
                }
            }
            Err(err) => Response::err(
                request.id,
                ipc::ErrorCode::BadRequest,
                format!("autostart.set needs {{\"enabled\":true}} or {{\"enabled\":false}}: {err}"),
            ),
        },
        "playback.pause" => forward(
            channels,
            latest,
            Command::Pause { id: request.id },
        ),
        "playback.resume" => forward(
            channels,
            latest,
            Command::Resume { id: request.id },
        ),
        "wallpaper.set" => match serde_json::from_value::<SetData>(request.data.clone()) {
            Err(err) => Response::err(
                request.id,
                ipc::ErrorCode::BadRequest,
                format!("wallpaper.set needs a JSON object: {err}"),
            ),
            Ok(data) => match (&data.path, &data.id) {
                (Some(_), Some(_)) => Response::err(
                    request.id,
                    ipc::ErrorCode::BadRequest,
                    "wallpaper.set takes either path or id, not both",
                ),
                (None, None) => Response::err(
                    request.id,
                    ipc::ErrorCode::BadRequest,
                    "wallpaper.set needs {\"path\":\"C:\\\\videos\\\\clip.mp4\"} (library ids arrive with the library, slice S3)",
                ),
                (None, Some(item_id)) => forward(
                    channels,
                    latest,
                    Command::SetWallpaperById {
                        id: request.id,
                        item_id: item_id.clone(),
                        monitor: data.monitor.clone(),
                    },
                ),
                (Some(path), None) => forward(
                    channels,
                    latest,
                    Command::SetWallpaper {
                        id: request.id,
                        path: path.clone(),
                        monitor: data.monitor.clone(),
                    },
                ),
            },
        },
        "wallpaper.clear" => {
            match serde_json::from_value::<MonitorData>(request.data.clone()) {
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::BadRequest,
                    format!("wallpaper.clear needs a JSON object: {err}"),
                ),
                Ok(data) => forward(
                    channels,
                    latest,
                    Command::ClearWallpaper {
                        id: request.id,
                        monitor: data.monitor,
                    },
                ),
            }
        }
        "wallpaper.restore" => forward(channels, latest, Command::Restore { id: request.id }),
        "library.list" => forward(channels, latest, Command::LibraryList { id: request.id }),
        "library.add" => match serde_json::from_value::<LibraryAddData>(request.data.clone()) {
            Ok(data) if data.paths.is_empty() => Response::err(
                request.id,
                ipc::ErrorCode::BadRequest,
                "library.add needs at least one path: {\"paths\":[\"C:\\\\videos\\\\clip.mp4\"]}",
            ),
            Ok(data) => forward(
                channels,
                latest,
                Command::LibraryAdd {
                    id: request.id,
                    paths: data.paths,
                },
            ),
            Err(err) => Response::err(
                request.id,
                ipc::ErrorCode::BadRequest,
                format!("library.add needs {{\"paths\":[...]}}: {err}"),
            ),
        },
        "library.remove" => {
            match serde_json::from_value::<LibraryRemoveData>(request.data.clone()) {
                Ok(data) if data.id.is_empty() => Response::err(
                    request.id,
                    ipc::ErrorCode::BadRequest,
                    "library.remove needs {\"id\":\"a3f2\"}",
                ),
                Ok(data) => forward(
                    channels,
                    latest,
                    Command::LibraryRemove {
                        id: request.id,
                        item_id: data.id,
                    },
                ),
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::BadRequest,
                    format!("library.remove needs {{\"id\":\"a3f2\"}}: {err}"),
                ),
            }
        }
        "library.prune" => forward(channels, latest, Command::LibraryPrune { id: request.id }),
        "assess" => match serde_json::from_value::<AssessRequest>(request.data.clone()) {
            Ok(data) => forward(
                channels,
                latest,
                Command::Assess {
                    id: request.id,
                    path: data.path,
                    monitor: data.monitor,
                },
            ),
            Err(err) => Response::err(
                request.id,
                ipc::ErrorCode::BadRequest,
                format!("assess data: {err}"),
            ),
        },
        "settings.set" => {
            match serde_json::from_value::<SettingsPatch>(request.data.clone()) {
                Ok(patch) => forward(
                    channels,
                    latest,
                    Command::SettingsSet {
                        id: request.id,
                        patch,
                    },
                ),
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::BadRequest,
                    format!("unreadable settings patch: {err}"),
                ),
            }
        }
        // `quit` needs the flush-ack handshake, which owns the connection
        // lifetime — handled here instead of through `forward`.
        "quit" => return quit_flow(handle, channels, latest, request.id),
        other => Response::err(
            request.id,
            ipc::ErrorCode::UnknownCommand,
            format!(
                "unknown command '{other}' (supported: ping, status, settings.get, settings.set, \
                 autostart.get, autostart.set, playback.pause, playback.resume, wallpaper.set, \
                 wallpaper.clear, wallpaper.restore, library.list, library.add, library.remove, \
                 library.prune, quit)"
            ),
        ),
    };
    let written = write_response(handle, &response);
    if response.flush_after {
        // The client has its answer (or is gone): ack so the main loop can
        // run the exit ladder and respawn (design-m2 §4 — the same flush
        // handshake as `quit`, design-m4 §2.3). Acking even a failed write
        // keeps the decision made in the main loop from stalling on a
        // client that disappeared mid-answer.
        let _ = channels.flush_acked.send(());
    }
    written
}

/// The listener's command set (`design-m5b.md` §4.5): `ping`, `status`, and
/// `quit` are answered right here on the pipe thread — there is no engine
/// and no main-loop command channel to forward to. Everything else gets the
/// honest "no engine is running" answer (the `settings`/`library` file ops
/// join in Pw2-S2, `design-m5b.md` §6). Returns `false` when the connection
/// must close (write failed).
fn dispatch_listener(
    handle: HANDLE,
    request: &ipc::Request,
    channels: &PipeChannels,
    latest: &mut Option<Arc<StatusSnapshot>>,
    started: Instant,
) -> bool {
    let response = match request.cmd.as_str() {
        "ping" => Response::ok(
            request.id,
            serde_json::to_value(Ping {
                pong: true,
                version: PROTOCOL_VERSION,
                uptime_ms: started.elapsed().as_millis() as u64,
            })
            .unwrap_or_else(|_| no_data()),
        ),
        "status" => {
            // Same drain as the daemon arm: the snapshot this thread was
            // given is the freshest one published.
            drain(channels, latest);
            match serde_json::to_value(listener_status(&snapshot_or_boot(latest))) {
                Ok(value) => Response::ok(request.id, value),
                Err(err) => Response::err(
                    request.id,
                    ipc::ErrorCode::Internal,
                    format!("could not build status: {err}"),
                ),
            }
        }
        // `design-m5b.md` §4.5: ok → flush ack → exit, no respawn. The
        // client has its answer before the main loop is asked to stop —
        // the same handshake as the daemon's `quit` (design-m4 §2.3); the
        // ack itself only reaches a loop that waits for it (the listener's
        // main loop reads it on the wake below).
        "quit" => {
            let response = Response::ok(request.id, no_data());
            let written = write_response(handle, &response);
            if response.ok {
                let _ = channels.flush_acked.send(());
                // Wake the main loop out of `GetMessageW` so it collects
                // the ack and exits now instead of on the next message.
                if let Some(hwnd) = control::find_existing() {
                    control::post(hwnd, WM_APP_PIPE);
                }
            }
            return written;
        }
        // Engine-only commands (`design-m5b.md` §4.5): honest text, exact
        // wording — the CLI prints it verbatim.
        _ => Response::err(
            request.id,
            ipc::ErrorCode::BadRequest,
            "Cascade is listening (battery mode) — no engine is running; `cascade run` starts it now"
                .to_string(),
        ),
    };
    write_response(handle, &response)
}

/// The listener's `status` (`design-m5b.md` D6): `build_status` fills
/// `engine:"running"` for the daemon — the listener has no engine and says
/// so. The rest of D6 comes from the snapshot itself (`mode:"listening"`,
/// `host.strategy:"none"`, monitors without playback, own process counters,
/// autostart from the Run key).
fn listener_status(snapshot: &StatusSnapshot) -> Status {
    let mut status = build_status(snapshot);
    status.engine = "stopped".to_string();
    status
}

/// Hand a mutating command to the main loop and wait for its response
/// (design-m4 §2.2: 2 s cap, then `busy`). Returns the response to write.
fn forward(
    channels: &PipeChannels,
    latest: &mut Option<Arc<StatusSnapshot>>,
    command: Command,
) -> Response {
    let id = command.id();
    match submit(channels, latest, command) {
        Some(response) => response,
        None => Response::err(
            id,
            ipc::ErrorCode::Busy,
            "the engine did not accept the request within 2 s — retry in a moment",
        ),
    }
}

/// Send one command to the main loop and block until it answers. Snapshot
/// messages that arrive while waiting update `latest`; responses to
/// earlier submits that timed out are stale and dropped.
fn submit(
    channels: &PipeChannels,
    latest: &mut Option<Arc<StatusSnapshot>>,
    command: Command,
) -> Option<Response> {
    let id = command.id();
    channels.commands.send(command).ok()?;
    // Wake the main loop so it drains the channel now instead of on the
    // next watchdog tick (design-m4 §2.2).
    if let Some(hwnd) = control::find_existing() {
        control::post(hwnd, WM_APP_PIPE);
    }
    let deadline = Instant::now() + SUBMIT_DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match channels.messages.recv_timeout(remaining) {
            Ok(ServerMsg::Snapshot(snapshot)) => *latest = Some(snapshot),
            Ok(ServerMsg::Response(response)) if response.id == id => return Some(response),
            // A stale answer from an earlier submit: drop it.
            Ok(ServerMsg::Response(_)) => {}
            // The main loop side is gone (process exit): report busy.
            Err(_) => return None,
        }
    }
}

/// `quit`: submit, write the response, then ack — the main loop starts the
/// exit ladder only after the client has its answer, so the daemon can
/// never tear the pipe down mid-write (design-m4 §2.3). A failed submit
/// answers `busy` without the ack: the main loop never saw the command.
fn quit_flow(
    handle: HANDLE,
    channels: &PipeChannels,
    latest: &mut Option<Arc<StatusSnapshot>>,
    id: u64,
) -> bool {
    let response = match submit(channels, latest, Command::Quit { id }) {
        Some(response) => response,
        None => Response::err(
            id,
            ipc::ErrorCode::Busy,
            "the engine did not accept quit within 2 s — retry, or use `cascade stop`",
        ),
    };
    let written = write_response(handle, &response);
    if response.ok {
        // The main loop waits for this before running the exit ladder.
        let _ = channels.flush_acked.send(());
    }
    written
}

/// Consume everything queued for this thread: snapshots update `latest`
/// (newest wins), stale responses are dropped.
fn drain(channels: &PipeChannels, latest: &mut Option<Arc<StatusSnapshot>>) {
    while let Ok(message) = channels.messages.try_recv() {
        match message {
            ServerMsg::Snapshot(snapshot) => *latest = Some(snapshot),
            ServerMsg::Response(_) => {}
        }
    }
}

fn snapshot_or_boot(latest: &Option<Arc<StatusSnapshot>>) -> Arc<StatusSnapshot> {
    match latest {
        Some(snapshot) => Arc::clone(snapshot),
        None => Arc::new(StatusSnapshot::boot()),
    }
}

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::new())
}

/// The Run-key view of autostart (`ipc.md` §3): shared by `status`,
/// `settings.get`, and `autostart.*` so all three always agree.
fn autostart_status() -> AutostartStatus {
    let state = registry::autostart_state(&exe_path());
    AutostartStatus {
        enabled: state.registered,
        windows_disabled: state.windows_disabled,
    }
}

/// Assemble `status`: snapshot (render-loop state) + counters this thread
/// may read on its own (`ipc.md` §4: the daemon fills `memory` itself).
fn build_status(snapshot: &StatusSnapshot) -> Status {
    Status {
        engine: "running".to_string(),
        mode: snapshot.mode.clone(),
        paused_reasons: snapshot.paused_reasons.clone(),
        monitors: snapshot
            .monitors
            .iter()
            .map(|monitor| {
                // Only the monitor the engine is attached to carries
                // playback fields; the rest keep their `None`s and the
                // wire omits them (`ipc.md` §4, M4 S3b).
                let playback = snapshot
                    .playback
                    .as_ref()
                    .filter(|playback| playback.monitor == monitor.id);
                cascade_core::ipc::MonitorStatus {
                    id: monitor.id.clone(),
                    rect: monitor.rect,
                    primary: monitor.primary,
                    source: playback.and_then(|playback| playback.source.clone()),
                    hwdec: playback.and_then(|playback| playback.hwdec),
                    fps: playback.and_then(|playback| playback.fps),
                    dropped: playback.and_then(|playback| playback.dropped),
                    video: playback.and_then(|playback| playback.video),
                    engine: playback.and_then(|playback| playback.engine.clone()),
                }
            })
            .collect(),
        memory: memory_status(),
        host: cascade_core::ipc::HostStatus {
            strategy: snapshot.host_strategy.clone(),
            repaint_level: snapshot.repaint_level,
        },
        autostart: autostart_status(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_boot_mode_is_documented() {
        // Not calling enumerate() here (GDI in a unit test): only the mode
        // string and the settings placeholder are asserted, which are the
        // wire-visible parts.
        let snapshot = StatusSnapshot {
            mode: "boot".to_string(),
            paused_reasons: Vec::new(),
            monitors: Vec::new(),
            playback: None,
            host_strategy: String::new(),
            repaint_level: 0,
            settings: Settings::default(),
        };
        assert_eq!(snapshot.mode, "boot");
        assert!(
            !snapshot.settings.autostart,
            "autostart is filled from the registry at the wire edge"
        );
    }

    #[test]
    fn listener_status_reports_no_engine_per_d6() {
        // design-m5b D6: the listener tells the truth about what it is —
        // `engine:"stopped"`, `mode:"listening"`, no host, no playback,
        // monitors listed by identity only.
        let snapshot = StatusSnapshot {
            mode: "listening".to_string(),
            paused_reasons: Vec::new(),
            monitors: Vec::new(),
            playback: None,
            host_strategy: "none".to_string(),
            repaint_level: 0,
            settings: Settings::default(),
        };
        let status = listener_status(&snapshot);
        assert_eq!(status.engine, "stopped");
        assert_eq!(status.mode, "listening");
        assert_eq!(status.host.strategy, "none");
        assert_eq!(status.host.repaint_level, 0);
        assert!(status.monitors.is_empty());
        assert!(status.paused_reasons.is_empty());
    }

    #[test]
    fn build_status_fills_only_the_attached_monitor_row() {
        // M4 S3b: the playback row lands on the monitor the engine is
        // attached to; every other monitor keeps its `None`s and the wire
        // omits them (ipc.md §4). An unobservable value stays `None` —
        // never a placeholder `false`.
        let monitors = vec![
            MonitorInfo {
                handle: 0,
                id: r"\\.\DISPLAY1".to_string(),
                rect: cascade_core::Rect::new(0, 0, 1366, 768),
                work: cascade_core::Rect::new(0, 0, 1366, 728),
                primary: true,
            },
            MonitorInfo {
                handle: 0,
                id: r"\\.\DISPLAY2".to_string(),
                rect: cascade_core::Rect::new(1366, 0, 1920, 1080),
                work: cascade_core::Rect::new(1366, 0, 1920, 1040),
                primary: false,
            },
        ];
        let snapshot = StatusSnapshot {
            mode: "attached".to_string(),
            paused_reasons: Vec::new(),
            monitors,
            playback: Some(PlaybackStatus {
                monitor: r"\\.\DISPLAY2".to_string(),
                source: Some(LibraryRef {
                    id: "a3f2".to_string(),
                    title: "Furina".to_string(),
                }),
                hwdec: None,
                fps: Some(59.94),
                dropped: Some(0),
                video: Some(VideoSize { w: 1920, h: 1080 }),
                engine: None,
            }),
            host_strategy: "workerw_child".to_string(),
            repaint_level: 1,
            settings: Settings::default(),
        };
        let status = build_status(&snapshot);
        let idle = &status.monitors[0];
        assert!(
            idle.source.is_none()
                && idle.hwdec.is_none()
                && idle.fps.is_none()
                && idle.dropped.is_none()
                && idle.video.is_none(),
            "a monitor without the engine must stay empty"
        );
        let playing = &status.monitors[1];
        assert_eq!(
            playing.source.as_ref().map(|source| source.id.as_str()),
            Some("a3f2")
        );
        assert_eq!(
            playing.source.as_ref().map(|source| source.title.as_str()),
            Some("Furina")
        );
        assert!(
            playing.hwdec.is_none(),
            "an unobservable hwdec must stay None, not false"
        );
        assert_eq!(playing.fps, Some(59.94));
        assert_eq!(playing.dropped, Some(0));
        assert_eq!(playing.video, Some(VideoSize { w: 1920, h: 1080 }));

        // Wire shape: known fields present, unknown field omitted.
        let line = encode_line(&status).expect("encode");
        assert!(!line.contains("hwdec"), "{line}");
        assert!(line.contains(r#""fps":59.94"#), "{line}");
        assert!(line.contains(r#""title":"Furina""#), "{line}");
        let back: Status = serde_json::from_str(&line).expect("decode");
        assert_eq!(back, status, "Status roundtrips");
    }

    #[test]
    fn command_ids_are_their_request_ids() {
        // The pipe thread matches responses by id and drops stale ones.
        assert_eq!(Command::Pause { id: 7 }.id(), 7);
        assert_eq!(Command::Resume { id: 8 }.id(), 8);
        assert_eq!(
            Command::SetWallpaper {
                id: 9,
                path: "a.mp4".to_string(),
                monitor: None
            }
            .id(),
            9
        );
        assert_eq!(
            Command::ClearWallpaper {
                id: 10,
                monitor: Some(r"\\.\DISPLAY1".to_string())
            }
            .id(),
            10
        );
        assert_eq!(Command::Restore { id: 11 }.id(), 11);
        assert_eq!(
            Command::SettingsSet {
                id: 12,
                patch: SettingsPatch::default()
            }
            .id(),
            12
        );
        assert_eq!(Command::Quit { id: 13 }.id(), 13);
    }

    #[test]
    fn stale_responses_are_dropped_by_drain() {
        // A response left behind by a submit that timed out must not be
        // mistaken for anything later.
        let (msg_tx, msg_rx) = std::sync::mpsc::channel::<ServerMsg>();
        let channels = PipeChannels {
            messages: msg_rx,
            commands: std::sync::mpsc::channel().0,
            flush_acked: std::sync::mpsc::channel().0,
        };
        msg_tx
            .send(ServerMsg::Response(Response::ok(99, no_data())))
            .expect("send");
        let mut latest: Option<Arc<StatusSnapshot>> = None;
        drain(&channels, &mut latest);
        assert!(
            latest.is_none(),
            "a response alone must not be mistaken for a snapshot"
        );
        assert!(channels.messages.try_recv().is_err(), "channel is drained");
    }
}
