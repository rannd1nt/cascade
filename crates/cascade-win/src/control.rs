//! Hidden top-level window that represents the running daemon.
//!
//! It gives us, without any IPC yet:
//! * single-instance detection (`FindWindowW`),
//! * a target for `cascade stop` / `cascade restore`,
//! * shutdown/logoff notifications,
//! * a place to run emergency cleanup from the window procedure.
//!
//! The real named-pipe protocol replaces the stop/restore messages in M4; the
//! window itself stays useful for session notifications.

// `HWND` is a raw pointer; callers pass handles returned by `find_existing` or
// by the daemon itself, and the OS validates them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, FindWindowW, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, IDC_ARROW, WM_APP, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_ENDSESSION,
    WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_POPUP, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use crate::wide;

pub const CONTROL_CLASS: &str = "CascadeDaemon";
pub const CONTROL_TITLE: &str = "Cascade";

/// Ask a running daemon to detach, repaint, and reattach without quitting.
pub const WM_CASCADE_RESTORE: u32 = WM_APP + 1;

/// Ask a running daemon to stop gracefully (repaint, then exit).
pub const WM_CASCADE_STOP: u32 = WM_CLOSE;

static RESTORE_REQUESTED: AtomicBool = AtomicBool::new(false);
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static DISPLAY_CHANGED: AtomicBool = AtomicBool::new(false);

/// Consume a pending restore request (called from the daemon message loop).
pub fn take_restore_request() -> bool {
    RESTORE_REQUESTED.swap(false, Ordering::AcqRel)
}

/// True once a stop has been requested; stays true until the process exits.
pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::Acquire)
}

/// Consume a pending display/resolution change (monitors may have moved).
pub fn take_display_change() -> bool {
    DISPLAY_CHANGED.swap(false, Ordering::AcqRel)
}

/// Current session lock state as last seen in `WM_WTSSESSION_CHANGE`.
static SESSION_LOCKED: AtomicBool = AtomicBool::new(false);
/// Set whenever the lock state changes; consumed by [`take_session_lock`].
static SESSION_CHANGED: AtomicBool = AtomicBool::new(false);

/// Register the control window for session lock/unlock notifications
/// (M3 pause slice, design-m3 §3). Returns the Win32 error on failure; the
/// caller degrades to running without pause-on-lock.
///
/// The registration is keyed to this window and is never unregistered: like
/// `SetConsoleCtrlHandler`, it lives exactly as long as the process (neither
/// `windows-sys` 0.61 nor `windows` 0.62 exposes
/// `WTSUnregisterSessionNotification`, and the exit path gains nothing from
/// a hand-written extern because the window dies with the process anyway).
pub fn register_session_notifications(hwnd: HWND) -> Result<(), u32> {
    // SAFETY: `hwnd` is our own live control window on this thread;
    // NOTIFY_FOR_THIS_SESSION limits events to the console session Cascade
    // draws into (design-m3 §3).
    let ok = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) };
    if ok == 0 {
        // SAFETY: reads the thread's last-error value.
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

/// Record a lock state (called from the window procedure; atomics only).
fn set_session_lock(locked: bool) {
    SESSION_LOCKED.store(locked, Ordering::Release);
    SESSION_CHANGED.store(true, Ordering::Release);
}

/// Consume a pending lock/unlock transition and return the new state, or
/// `None` when nothing changed since the last call. The main loop applies
/// the rule (design-m3 §3).
pub fn take_session_lock() -> Option<bool> {
    if SESSION_CHANGED.swap(false, Ordering::AcqRel) {
        Some(SESSION_LOCKED.load(Ordering::Acquire))
    } else {
        None
    }
}

/// Current lock state without consuming the transition — what the IPC
/// `status` snapshot reports (M4 S1).
pub fn session_locked() -> bool {
    SESSION_LOCKED.load(Ordering::Acquire)
}

pub fn find_existing() -> Option<HWND> {
    let class = wide::to_wide(CONTROL_CLASS);
    // SAFETY: class name is NUL-terminated; null title matches any title.
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    (!hwnd.is_null()).then_some(hwnd)
}

/// Post `msg` to the daemon's control window.
pub fn post(hwnd: HWND, msg: u32) -> bool {
    // SAFETY: the handle is looked up by the caller; PostMessageW validates it.
    unsafe { PostMessageW(hwnd, msg, 0, 0) != 0 }
}

pub struct ControlWindow {
    hwnd: HWND,
}

impl ControlWindow {
    pub fn create() -> Result<Self, u32> {
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = wide::to_wide(CONTROL_CLASS);
        let title = wide::to_wide(CONTROL_TITLE);
        let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(control_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: cursor,
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        // SAFETY: `wc` is fully initialized and strings live for the call.
        let atom = unsafe { RegisterClassExW(&wc) };
        if atom == 0 {
            // SAFETY: reads the thread's last-error value.
            let code = unsafe { GetLastError() };
            const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
            if code != ERROR_CLASS_ALREADY_EXISTS {
                return Err(code);
            }
        }

        // Hidden top-level window: no taskbar entry (TOOLWINDOW), never
        // activated (NOACTIVATE), never shown. It still receives session
        // notifications, which message-only windows would not.
        // SAFETY: class/title are NUL-terminated; parent is HWND_DESKTOP.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            // SAFETY: reads the thread's last-error value.
            return Err(unsafe { GetLastError() });
        }
        Ok(Self { hwnd })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Request a graceful stop. The daemon repaints before exiting.
    pub fn request_close(&self) -> bool {
        post(self.hwnd, WM_CLOSE)
    }
}

unsafe extern "system" fn control_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => {
            QUIT_REQUESTED.store(true, Ordering::Release);
            PostQuitMessage(0);
            0
        }
        WM_DESTROY => {
            QUIT_REQUESTED.store(true, Ordering::Release);
            PostQuitMessage(0);
            0
        }
        WM_DISPLAYCHANGE => {
            DISPLAY_CHANGED.store(true, Ordering::Release);
            0
        }
        // Session lock/unlock (M3 pause slice, design-m3 §3): flags only —
        // the main loop evaluates the rule and calls into the backend.
        // `wParam` is the change code, `lParam` the session id (Win32 doc).
        WM_WTSSESSION_CHANGE => {
            match wparam as u32 {
                WTS_SESSION_LOCK => set_session_lock(true),
                WTS_SESSION_UNLOCK => set_session_lock(false),
                _ => {}
            }
            0
        }
        // Never block shutdown or logoff.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            // wParam != 0 means the session is really ending.
            if wparam != 0 {
                crate::host::emergency_cleanup(2);
            }
            0
        }
        WM_CASCADE_RESTORE => {
            RESTORE_REQUESTED.store(true, Ordering::Release);
            0
        }
        _ => {
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The lock statics are process-global, so this is the only test that
    // touches them: it asserts the whole transition sequence in one place.
    #[test]
    fn session_lock_transitions_are_reported_once() {
        assert_eq!(take_session_lock(), None);

        set_session_lock(true);
        assert_eq!(take_session_lock(), Some(true));
        // Consumed: a second take sees nothing.
        assert_eq!(take_session_lock(), None);

        set_session_lock(false);
        assert_eq!(take_session_lock(), Some(false));
        assert_eq!(take_session_lock(), None);

        // A lock and an unlock between two takes converge on the final
        // state (the backend never saw the intermediate one either).
        set_session_lock(true);
        set_session_lock(false);
        assert_eq!(take_session_lock(), Some(false));
        assert_eq!(take_session_lock(), None);
    }
}
