//! Hidden top-level window that represents the running daemon.
//!
//! It gives us, without any IPC yet:
//! * single-instance detection (`FindWindowW`),
//! * a target for `fontaine stop` / `fontaine restore`,
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
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, FindWindowW, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, IDC_ARROW, WM_APP, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_ENDSESSION,
    WM_QUERYENDSESSION, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::wide;

pub const CONTROL_CLASS: &str = "FontaineDaemon";
pub const CONTROL_TITLE: &str = "Fontaine";

/// Ask a running daemon to detach, repaint, and reattach without quitting.
pub const WM_FONTAINE_RESTORE: u32 = WM_APP + 1;

/// Ask a running daemon to stop gracefully (repaint, then exit).
pub const WM_FONTAINE_STOP: u32 = WM_CLOSE;

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
        // Never block shutdown or logoff.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            // wParam != 0 means the session is really ending.
            if wparam != 0 {
                crate::host::emergency_cleanup(2);
            }
            0
        }
        WM_FONTAINE_RESTORE => {
            RESTORE_REQUESTED.store(true, Ordering::Release);
            0
        }
        _ => {
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}
