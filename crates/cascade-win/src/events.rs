//! Server→client event stream.
//!
//! A second named pipe, `\\.\pipe\cascade-evt-{session}`, separate from
//! the control pipe so the control path keeps its single-instance,
//! one-client-at-a-time design untouched (CLI-first). The
//! server runs on one process-scoped thread with two phases: block in
//! `ConnectNamedPipe` while nobody subscribes, then park on a condvar
//! and write coalesced event lines until the client goes away.
//!
//! Emission (`emit_*`) is called from the daemon's own threads: one
//! relaxed atomic load says whether anyone is listening, so with no
//! subscriber the call does no locking, no waking, and no I/O — idle
//! event work is 0. Delivery is paced to at most
//! 10 lines/second on the serve thread, never on an
//! emitting thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use cascade_core::client::event_server_name;
use cascade_core::ipc::{encode_line, Event};
use cascade_core::library::LibraryItem;
use cascade_core::PROTOCOL_VERSION;
use serde::Serialize;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_NO_DATA,
    ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_OUTBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::derr;
use crate::dlog;
use crate::wide;

/// Outbound buffer per pipe instance — event lines are small (a `Status`
/// fragment, a full library list); the same size the control pipe uses.
const BUFFER_BYTES: u32 = 4096;
/// Minimum gap between delivered lines: the wire cap of 10 events per
/// second, enforced here so emitting threads never sleep.
const LINE_PACING: Duration = Duration::from_millis(100);
/// How long `shutdown()` waits for the last line to leave the process.
/// Bounded on purpose: the exit ladder must never wait on a client
/// and the client treats a missing `shutdown` exactly
/// like a broken pipe.
const SHUTDOWN_FLUSH_WAIT: Duration = Duration::from_millis(200);
/// `log` events buffered before the oldest is dropped: "surface once"
/// diagnostics, newest wins.
const LOG_CAP: usize = 8;
/// Respawn contention budget for the event pipe: the same wait the
/// control pipe uses (`daemon.rs` `RESPAWN_PIPE_WAIT`), because a dying
/// parent may still hold its instance for a moment.
const CREATE_WAIT: Duration = Duration::from_secs(5);
const CREATE_POLL: Duration = Duration::from_millis(100);
/// Pause after an unexpected accept error so a broken handle degrades to
/// a slow retry instead of a hot loop.
const RETRY_PAUSE: Duration = Duration::from_millis(250);

// payloads -------------------------------------------------------------

#[derive(Serialize)]
struct StateData<'a> {
    mode: &'a str,
    paused_reasons: &'a [String],
}

#[derive(Serialize)]
struct LibraryData<'a> {
    items: &'a [LibraryItem],
}

#[derive(Serialize)]
struct LogData<'a> {
    level: &'a str,
    message: &'a str,
}

fn wire_event(name: &str, data: serde_json::Value) -> Event {
    Event {
        v: PROTOCOL_VERSION,
        event: name.to_string(),
        data,
    }
}

/// Serialize a plain payload, or log and drop the event. Payloads here
/// are simple data that cannot realistically fail; a failure must never
/// panic a background daemon.
fn payload<T: Serialize>(value: &T) -> Option<serde_json::Value> {
    match serde_json::to_value(value) {
        Ok(data) => Some(data),
        Err(err) => {
            derr!("ipc: could not encode an event payload: {err}");
            None
        }
    }
}

// pending slots (coalescing) -------------------------------------------

/// What waits for the serve thread. Typed slots instead of a queue:
/// `state` and `library.changed` carry *current* state, so the newest
/// value always replaces the older one and coalescing is lossless
///
#[derive(Default)]
struct Pending {
    state: Option<Event>,
    library: Option<Event>,
    logs: VecDeque<Event>,
    shutdown: bool,
    /// Set by the serve thread after the `shutdown` line is written;
    /// the flush wait polls this with a deadline.
    shutdown_written: bool,
}

impl Pending {
    fn has_work(&self) -> bool {
        self.state.is_some() || self.library.is_some() || !self.logs.is_empty() || self.shutdown
    }

    /// Drain everything queued. `shutdown` goes last so a subscriber
    /// receives the pending state before the daemon disappears, and a
    /// batch containing it skips line pacing — the process is exiting.
    fn take_batch(&mut self) -> Batch {
        let mut events = Vec::new();
        if let Some(event) = self.state.take() {
            events.push(event);
        }
        if let Some(event) = self.library.take() {
            events.push(event);
        }
        events.extend(self.logs.drain(..));
        let had_shutdown = std::mem::take(&mut self.shutdown);
        if had_shutdown {
            events.push(wire_event(
                "shutdown",
                serde_json::Value::Object(serde_json::Map::new()),
            ));
        }
        Batch {
            events,
            pace: !had_shutdown,
        }
    }
}

struct Batch {
    events: Vec<Event>,
    pace: bool,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    /// New work for the serve thread.
    wake: Condvar,
    /// Acknowledges the written `shutdown` line.
    acked: Condvar,
    /// True only while a subscriber is connected; the emit fast path
    /// reads this first and returns without locking when false.
    subscribers: AtomicBool,
}

struct EventBus {
    shared: Arc<Shared>,
}

/// Process-scoped bus: the handle and thread live
/// past `daemon::run`'s locals so `ExitSignal::drop` can still flush the
/// `shutdown` line. Empty in the listener and in tests → every emit and
/// the flush are no-ops there.
static BUS: OnceLock<EventBus> = OnceLock::new();

/// Recover a mutex even if some other thread panicked while holding it:
/// every critical section here is a plain field assignment, so the data
/// is structurally intact (no `unwrap` in library code).
fn lock(shared: &Shared) -> MutexGuard<'_, Pending> {
    shared
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn push(edit: impl FnOnce(&mut Pending)) {
    let Some(bus) = BUS.get() else {
        return;
    };
    // No subscriber → no work at all: the isolation invariant's "idle
    // event work is 0".
    if !bus.shared.subscribers.load(Ordering::Relaxed) {
        return;
    }
    let mut pending = lock(&bus.shared);
    edit(&mut pending);
    drop(pending);
    bus.shared.wake.notify_one();
}

// emission points ----------------------------------

/// `state`: mode / pause reasons / playback identity changed. Numeric
/// counters are deliberately absent — they tick on the 1 s `status` poll
///
pub fn emit_state(mode: &str, paused_reasons: &[String]) {
    let Some(data) = payload(&StateData {
        mode,
        paused_reasons,
    }) else {
        return;
    };
    push(|pending| pending.state = Some(wire_event("state", data)));
}

/// `library.changed`: the payload is the **full** list, so coalescing
/// several rapid mutations into the newest one loses nothing.
pub fn emit_library_changed(items: &[LibraryItem]) {
    let Some(data) = payload(&LibraryData { items }) else {
        return;
    };
    push(|pending| pending.library = Some(wire_event("library.changed", data)));
}

/// `log`: a warning the UI should surface once. Call sites
/// are curated — internal diagnostics are not user-facing (
///
pub fn emit_log(level: &str, message: &str) {
    let Some(data) = payload(&LogData { level, message }) else {
        return;
    };
    push(|pending| {
        if pending.logs.len() >= LOG_CAP {
            pending.logs.pop_front();
        }
        pending.logs.push_back(wire_event("log", data));
    });
}

/// `shutdown` + bounded flush: called from `ExitSignal::drop`, the one
/// point every daemon exit path passes through. Returns immediately when
/// nobody listens, and never blocks longer than
/// [`SHUTDOWN_FLUSH_WAIT`].
pub fn shutdown() {
    let Some(bus) = BUS.get() else {
        return;
    };
    if !bus.shared.subscribers.load(Ordering::Relaxed) {
        return;
    }
    {
        let mut pending = lock(&bus.shared);
        pending.shutdown = true;
    }
    bus.shared.wake.notify_one();

    let deadline = Instant::now() + SHUTDOWN_FLUSH_WAIT;
    let mut pending = lock(&bus.shared);
    loop {
        if pending.shutdown_written {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            derr!("ipc: event shutdown flush timed out");
            return;
        }
        pending = bus
            .shared
            .acked
            .wait_timeout(pending, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
}

// server ---------------------------------------------------------------

/// Start the event stream. Called once per daemon, **after** the control
/// pipe is ours (the control pipe stays the single-instance guard).
/// Every failure degrades to today's poll-only behaviour with a log
/// line: no events is a lesser state, never a broken desktop
///
pub fn init() {
    if BUS.get().is_some() {
        return;
    }
    let handle = match create_event_pipe() {
        Ok(handle) => handle,
        Err(code) => {
            derr!("ipc: event pipe unavailable (Win32 error {code}); clients poll instead");
            return;
        }
    };
    let shared = Arc::new(Shared::default());
    if BUS
        .set(EventBus {
            shared: Arc::clone(&shared),
        })
        .is_err()
    {
        // Defensive: init runs once per process (single instance).
        // SAFETY: handle came from CreateNamedPipeW, closed once here.
        unsafe { CloseHandle(handle) };
        return;
    }
    // HANDLE is not `Send`; carry the value as an integer and cast back
    // inside the thread — the only code that ever dereferences it.
    let handle_addr = handle as isize;
    match std::thread::Builder::new()
        .name("cascade-evt".to_string())
        .spawn(move || serve(handle_addr, shared))
    {
        Ok(_) => dlog!("ipc: event stream listening on {}", event_server_name()),
        Err(err) => {
            // The thread never started; close the handle ourselves.
            // SAFETY: handle came from CreateNamedPipeW, closed once here.
            unsafe { CloseHandle(handle) };
            derr!("ipc: could not start the event thread: {err}");
        }
    }
}

/// Create the event pipe, retrying only the contention a respawn child
/// can legitimately meet (`ERROR_ACCESS_DENIED` from
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` while the dying parent still holds
/// its instance). Anything else fails fast.
fn create_event_pipe() -> Result<HANDLE, u32> {
    let deadline = Instant::now() + CREATE_WAIT;
    loop {
        match try_create_event_pipe() {
            Ok(handle) => return Ok(handle),
            Err(code) if code == ERROR_ACCESS_DENIED && Instant::now() < deadline => {
                std::thread::sleep(CREATE_POLL);
            }
            Err(code) => return Err(code),
        }
    }
}

fn try_create_event_pipe() -> Result<HANDLE, u32> {
    let name = wide::to_wide(&event_server_name());
    // SAFETY: `name` is NUL-terminated; a null SECURITY_ATTRIBUTES
    // gives the default DACL (same user only); the buffer
    // sizes are kernel-side counts, not pointers.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            // One server instance = one subscriber at a time
            // a second waits with normal backoff.
            1,
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
    Ok(handle)
}

/// The one process-scoped serve thread: wait for a subscriber, then
/// deliver until its writes fail, then wait again.
fn serve(raw: isize, shared: Arc<Shared>) {
    let handle = raw as HANDLE;
    let mut last_line: Option<Instant> = None;
    loop {
        // Phase A: block until someone subscribes (0 CPU while idle).
        // SAFETY: null OVERLAPPED = synchronous accept on our own handle.
        let ok = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        // SAFETY: reads the thread's last-error value.
        let err = if ok != 0 {
            0
        } else {
            unsafe { GetLastError() }
        };
        match err {
            0 | ERROR_PIPE_CONNECTED => {}
            ERROR_NO_DATA | ERROR_BROKEN_PIPE => {
                // SAFETY: our own instance; disconnect returns it to the
                // listening state so the next connect can succeed.
                unsafe { DisconnectNamedPipe(handle) };
                continue;
            }
            code => {
                derr!("ipc: event accept failed (Win32 error {code}); retrying");
                std::thread::sleep(RETRY_PAUSE);
                continue;
            }
        }

        // New subscriber: discard anything queued for a previous one —
        // events are not replayed; the client resyncs
        // statelessly right after connecting.
        {
            let mut pending = lock(&shared);
            *pending = Pending::default();
        }
        shared.subscribers.store(true, Ordering::Release);

        // Phase B: deliver until a write fails.
        let mut connected = true;
        while connected {
            let batch = {
                let mut pending = lock(&shared);
                while !pending.has_work() {
                    pending = shared
                        .wake
                        .wait(pending)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                pending.take_batch()
            };
            for line_event in &batch.events {
                if batch.pace {
                    if let Some(previous) = last_line {
                        if let Some(wait) = LINE_PACING.checked_sub(previous.elapsed()) {
                            // Only this thread ever sleeps: emitting
                            // threads just queue and return.
                            std::thread::sleep(wait);
                        }
                    }
                }
                let Ok(mut line) = encode_line(line_event) else {
                    derr!(
                        "ipc: could not encode a {} event; dropped",
                        line_event.event
                    );
                    continue;
                };
                line.push('\n');
                if !write_all(handle, line.as_bytes()) {
                    connected = false;
                    break;
                }
                last_line = Some(Instant::now());
                if line_event.event == "shutdown" {
                    let mut pending = lock(&shared);
                    pending.shutdown_written = true;
                    shared.acked.notify_all();
                }
            }
        }

        shared.subscribers.store(false, Ordering::Release);
        // SAFETY: our own connected instance; disconnect lets the next
        // subscriber (or this one reconnecting) enter phase A again.
        unsafe { DisconnectNamedPipe(handle) };
    }
}

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let mut written: u32 = 0;
        // SAFETY: valid buffer; null OVERLAPPED = synchronous write on
        // our own outbound instance.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state_event(mode: &str) -> Event {
        wire_event(
            "state",
            payload(&StateData {
                mode,
                paused_reasons: &[],
            })
            .expect("plain state payload encodes"),
        )
    }

    #[test]
    fn state_and_library_slots_coalesce_to_the_newest() {
        let mut pending = Pending {
            state: Some(state_event("paused")),
            library: None,
            logs: VecDeque::new(),
            shutdown: false,
            shutdown_written: false,
        };
        pending.state = Some(state_event("attached"));
        pending.library = Some(wire_event(
            "library.changed",
            serde_json::json!({"items":[]}),
        ));
        pending.library = Some(wire_event(
            "library.changed",
            serde_json::json!({"items":[{"id":"a"}]}),
        ));
        let batch = pending.take_batch();
        assert_eq!(batch.events.len(), 2, "one line per slot, newest payload");
        assert_eq!(batch.events[0].event, "state");
        assert_eq!(batch.events[0].data["mode"], "attached");
        assert_eq!(batch.events[1].event, "library.changed");
        assert_eq!(batch.events[1].data["items"][0]["id"], "a");
        assert!(batch.pace, "a normal batch is rate-paced");
        assert!(!pending.has_work(), "batch drains the slots");
    }

    #[test]
    fn log_queue_keeps_the_newest_and_caps_the_oldest() {
        let mut pending = Pending::default();
        for index in 0..LOG_CAP + 3 {
            let data = payload(&LogData {
                level: "warn",
                message: &format!("m{index}"),
            })
            .expect("plain log payload encodes");
            if pending.logs.len() >= LOG_CAP {
                pending.logs.pop_front();
            }
            pending.logs.push_back(wire_event("log", data));
        }
        let batch = pending.take_batch();
        assert_eq!(batch.events.len(), LOG_CAP, "cap holds under a burst");
        assert_eq!(batch.events[0].data["message"], "m3", "oldest were dropped");
        assert_eq!(
            batch.events[LOG_CAP - 1].data["message"],
            format!("m{}", LOG_CAP + 2),
            "newest survived"
        );
    }

    #[test]
    fn shutdown_goes_last_and_skips_pacing() {
        let mut pending = Pending {
            state: Some(state_event("paused")),
            library: None,
            logs: VecDeque::new(),
            shutdown: true,
            shutdown_written: false,
        };
        let batch = pending.take_batch();
        assert_eq!(
            batch.events.last().map(|event| event.event.as_str()),
            Some("shutdown"),
            "state is delivered before the daemon disappears"
        );
        assert!(!batch.pace, "the exit path must not sleep between lines");
        assert!(!pending.shutdown, "the flag is consumed exactly once");
        assert!(!pending.has_work(), "and the batch drains everything");
    }

    #[test]
    fn emits_without_a_bus_are_noops() {
        // The listener and unit tests never call `init`: emission must
        // be a silent no-op, not a panic.
        emit_state("attached", &[]);
        emit_library_changed(&[]);
        emit_log("warn", "test");
        shutdown();
    }
}
