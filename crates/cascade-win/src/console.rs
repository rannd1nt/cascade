//! Console handling for a GUI-subsystem binary.
//!
//! `cascade.exe` is linked as a GUI-subsystem executable: launching it must
//! never create a console window. A console-subsystem build was measured to
//! flash a Terminal window and steal focus for ~100 ms on every Run-key start
//! (see `docs/spike-m0.md`, "M1-F corrections"), which fails M1-F.
//!
//! The CLI still has to print, so `main` prepares stdio per role:
//!
//! | role                                | stdio                                  |
//! |-------------------------------------|----------------------------------------|
//! | std handles already valid           | untouched (pipes, files, redirection)  |
//! | an existing console (parent or own) | joined; NULL handles reopened on `CONOUT$` |
//! | CLI without any console (double-click) | `AllocConsole`, then `CONOUT$`       |
//! | daemon without a console            | `NUL` sink — never a window            |
//!
//! Joining a console never creates a window: that console already exists.
//! Only the explicit `AllocConsole` for a double-clicked CLI does, which is
//! what a console application is expected to do.

use std::fs::OpenOptions;
use std::mem;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, GetConsoleWindow, GetStdHandle, SetStdHandle,
    ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

/// Best-effort line on stdout. Unlike `println!` this never panics: during a
/// console close (CTRL_CLOSE) the write can fail, and a panic on the exit
/// path would skip the repaint and the state save.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// Best-effort line on stderr; see [`dlog`](crate::dlog).
#[macro_export]
macro_rules! derr {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

/// Which stdio strategy [`prepare_stdio`] selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StdioMode {
    /// Standard handles were already valid (redirect or inheritance).
    Inherited,
    /// Joined a console that already existed.
    Attached,
    /// Created a fresh console for a double-clicked CLI.
    Allocated,
    /// No console and none created; output goes to `NUL`.
    Sink,
}

/// Prepare stdio for this process. Call once, first thing in `main`, before
/// anything prints.
///
/// `daemon` selects the silent strategy: the long-running role must never
/// create a console window, so without an existing console its output is
/// discarded instead.
pub fn prepare_stdio(daemon: bool) -> StdioMode {
    // Join the parent's console when it has one. This cannot show a window:
    // the console already exists. Without a parent console it simply fails.
    // SAFETY: no arguments are read and no pointers are passed.
    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 };
    // The console may already be ours (`CREATE_NEW_CONSOLE`); joining is then
    // unnecessary, but the window exists so output may go there.
    // SAFETY: takes no arguments; the return value is only compared.
    let have_console = attached || unsafe { !GetConsoleWindow().is_null() };

    // Valid standard handles (redirect to a file or a pipe) are never touched.
    if stdio_valid() {
        return StdioMode::Inherited;
    }

    // A console exists: a GUI-subsystem process starts with NULL standard
    // handles, so point the missing ones at it. Rust's `println!` resolves the
    // handle through `GetStdHandle` on write, so setting it here is enough.
    if have_console {
        ensure_stdio("CONOUT$");
        return StdioMode::Attached;
    }

    // No console at all. The daemon stays silent; a CLI is a double-click and
    // gets the console a console application would normally create.
    if daemon || unsafe { AllocConsole() } == 0 {
        ensure_stdio("NUL");
        return StdioMode::Sink;
    }
    ensure_stdio("CONOUT$");
    StdioMode::Allocated
}

/// `true` when both output standard handles refer to something writable.
fn stdio_valid() -> bool {
    handle_valid(STD_OUTPUT_HANDLE) && handle_valid(STD_ERROR_HANDLE)
}

/// `true` when this process has (or can join) a console: valid redirected
/// handles, the parent's console, or an own console window.
///
/// May attach to the parent console as a side effect — joining never creates
/// a window (see the module docs). Used by the bare-invocation flow
/// (design-m4 §6.2) *before* [`prepare_stdio`] decides whether to allocate
/// one, so a double-click is distinguishable from a typed command.
pub fn have_console() -> bool {
    if stdio_valid() {
        return true;
    }
    // SAFETY: no arguments are read and no pointers are passed.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 } {
        return true;
    }
    // SAFETY: takes no arguments; the return value is only compared.
    unsafe { !GetConsoleWindow().is_null() }
}

fn handle_valid(id: u32) -> bool {
    // SAFETY: `id` is a constant std-handle identifier; the returned handle
    // is only compared, never dereferenced here.
    let handle = unsafe { GetStdHandle(id) };
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

/// Point every invalid output standard handle at `path` (`CONOUT$` or `NUL`).
fn ensure_stdio(path: &str) {
    if stdio_valid() {
        return;
    }
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        // No console device (or no rights): leave the handles alone rather
        // than fail startup; the caller's prints may then be lost.
        Err(_) => return,
    };
    let raw = file.as_raw_handle();
    unsafe {
        // SAFETY: `raw` is an open handle; `SetStdHandle` only stores the
        // value for later `GetStdHandle` calls and does not take ownership.
        if !handle_valid(STD_OUTPUT_HANDLE) {
            SetStdHandle(STD_OUTPUT_HANDLE, raw);
        }
        if !handle_valid(STD_ERROR_HANDLE) {
            SetStdHandle(STD_ERROR_HANDLE, raw);
        }
    }
    // `SetStdHandle` does not duplicate the handle: it must stay open for the
    // whole process lifetime, so it is deliberately never closed.
    mem::forget(file);
}
