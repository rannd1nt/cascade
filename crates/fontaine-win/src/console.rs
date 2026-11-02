//! Console handling for the daemon role.
//!
//! `fontaine.exe` is a console binary so that the CLI (`stop`, `doctor`, …)
//! can print normally. The daemon, however, is often started by the `Run` key
//! or by double-clicking the executable, and in both cases Windows attaches a
//! fresh console window that the user did not ask for. Detaching is only safe
//! when *we own* the console; when the daemon was started from a terminal the
//! console belongs to the shell and must be left alone.

use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};

/// Detach from the console when this process is its only client.
///
/// Returns `true` when a private console window was detached. Long-running
/// hosts should call this once at startup; it is a no-op when stdio is
/// redirected (no console) or shared with a shell.
pub fn detach_if_private() -> bool {
    let mut pids = [0u32; 2];
    // SAFETY: the buffer and its length are valid; the call only fills it.
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if count == 1 {
        // SAFETY: this process is the only console client, so closing it does
        // not disturb anyone else.
        unsafe { FreeConsole() };
        return true;
    }
    false
}
