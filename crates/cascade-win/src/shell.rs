//! Finding the desktop's wallpaper surface.
//!
//! The desktop window hierarchy is undocumented and has changed between
//! Windows releases. Two layouts are handled:
//!
//! * **Classic** (Windows 7 … 11 23H2) — a top-level `WorkerW` holds
//!   `SHELLDLL_DefView`, and the wallpaper `WorkerW` is its sibling.
//! * **Raised desktop** (Windows 11 24H2 / build 26100+) — `Progman` is created
//!   with `WS_EX_NOREDIRECTIONBITMAP`, `SHELLDLL_DefView` is a `WS_EX_LAYERED`
//!   child of it, and the wallpaper is painted by a `WorkerW` that is also a
//!   child of `Progman`. A child of that `WorkerW` is drawn *under* the
//!   wallpaper, so live content has to live in a layered child of `Progman`
//!   that is z-ordered between `DefView` and `WorkerW`.
//!
//! We never enumerate or modify other applications' windows beyond reading the
//! class name of desktop-related windows.

// `HWND` is a raw pointer. Callers only pass handles returned by these helpers
// or by Win32 itself; the OS validates them, so marking these safe helpers
// `unsafe` would push noise into every call site without gaining anything.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetWindow, GetWindowLongPtrW,
    SendMessageTimeoutW, GWL_EXSTYLE, GW_HWNDNEXT, SMTO_ABORTIFHUNG, SMTO_NORMAL, WS_EX_LAYERED,
    WS_EX_NOREDIRECTIONBITMAP,
};

use crate::wide;

/// `0x052C` asks Explorer for the "raised desktop" state. It is not a
/// documented message; both known variants are tried.
const SPAWN_WORKER_MESSAGE: u32 = 0x052C;

const CLASS_PROGMAN: &str = "Progman";
const CLASS_WORKERW: &str = "WorkerW";
const CLASS_DEFVIEW: &str = "SHELLDLL_DefView";

/// Where host windows have to be placed so that they are visible behind icons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopMode {
    /// Host windows are children of the wallpaper `WorkerW`.
    WorkerWChild,
    /// Windows 11 24H2+: host windows are layered children of `Progman`,
    /// z-ordered below `DefView` and above the wallpaper `WorkerW`.
    LayeredProgmanChild,
}

impl DesktopMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkerWChild => "workerw_child",
            Self::LayeredProgmanChild => "layered_progman_child",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ShellSurface {
    pub progman: HWND,
    /// The window that paints the wallpaper (and the target of the repaint
    /// ladder). Host windows are *not* children of it in raised mode.
    pub workerw: HWND,
    /// Icon view, when Explorer exposes one. Host windows go directly below it.
    pub defview: HWND,
    pub mode: DesktopMode,
}

impl ShellSurface {
    /// Parent that host windows must be created under.
    pub fn host_parent(&self) -> HWND {
        match self.mode {
            DesktopMode::WorkerWChild => self.workerw,
            DesktopMode::LayeredProgmanChild => self.progman,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ShellError {
    ProgmanTimeout { waited_ms: u128 },
    NoWallpaperSurface,
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProgmanTimeout { waited_ms } => write!(
                f,
                "Explorer's Progman window did not appear within {waited_ms} ms"
            ),
            Self::NoWallpaperSurface => write!(
                f,
                "Progman exists but no wallpaper WorkerW was created (Explorer too new or a third-party shell)"
            ),
        }
    }
}

impl std::error::Error for ShellError {}

pub fn class_name(hwnd: HWND) -> String {
    if hwnd.is_null() {
        return String::new();
    }
    let mut buf = [0u16; 256];
    // SAFETY: buffer and length are valid; the API NUL-terminates on success.
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        String::new()
    } else {
        wide::from_wide(&buf[..len as usize])
    }
}

/// Extended window style, or 0 when the window is gone.
pub fn ex_style(hwnd: HWND) -> u32 {
    if hwnd.is_null() {
        return 0;
    }
    // SAFETY: GWL_EXSTYLE does not dereference pointers; stale handles return 0.
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

pub fn progman() -> Option<HWND> {
    let class = wide::to_wide(CLASS_PROGMAN);
    // SAFETY: class name is NUL-terminated; null window name is allowed.
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    (!hwnd.is_null()).then_some(hwnd)
}

fn first_child_of_class(parent: HWND, class: &str) -> Option<HWND> {
    let class = wide::to_wide(class);
    // SAFETY: both strings are NUL-terminated and `parent` is a valid window.
    let hwnd = unsafe {
        FindWindowExW(
            parent,
            std::ptr::null_mut(),
            class.as_ptr(),
            std::ptr::null(),
        )
    };
    (!hwnd.is_null()).then_some(hwnd)
}

pub fn defview(progman: HWND) -> Option<HWND> {
    if let Some(view) = first_child_of_class(progman, CLASS_DEFVIEW) {
        return Some(view);
    }
    // Classic layout: DefView may live inside one of Progman's WorkerW children.
    let worker_class = wide::to_wide(CLASS_WORKERW);
    let view_class = wide::to_wide(CLASS_DEFVIEW);
    let mut child = std::ptr::null_mut();
    loop {
        // SAFETY: `child` starts null and is always a window returned by the
        // same enumeration; class names are NUL-terminated.
        child = unsafe { FindWindowExW(progman, child, worker_class.as_ptr(), std::ptr::null()) };
        if child.is_null() {
            return None;
        }
        // SAFETY: as above.
        let view = unsafe {
            FindWindowExW(
                child,
                std::ptr::null_mut(),
                view_class.as_ptr(),
                std::ptr::null(),
            )
        };
        if !view.is_null() {
            return Some(view);
        }
    }
}

fn ask_explorer_to_spawn_worker(progman: HWND, wparam: usize, lparam: isize) {
    let mut result = 0usize;
    // SAFETY: valid window handle and out pointer; SMTO_ABORTIFHUNG keeps a
    // hung Explorer from blocking us for more than the timeout.
    unsafe {
        SendMessageTimeoutW(
            progman,
            SPAWN_WORKER_MESSAGE,
            wparam,
            lparam,
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            1_000,
            &mut result,
        );
    }
}

fn workerw_children(progman: HWND) -> Vec<HWND> {
    let class = wide::to_wide(CLASS_WORKERW);
    let mut out = Vec::new();
    let mut child = std::ptr::null_mut();
    loop {
        // SAFETY: standard FindWindowExW iteration; class name is NUL-terminated.
        child = unsafe { FindWindowExW(progman, child, class.as_ptr(), std::ptr::null()) };
        if child.is_null() {
            break;
        }
        out.push(child);
    }
    out
}

fn find_wallpaper_workerw(progman: HWND) -> Option<HWND> {
    // 24H2+: the wallpaper WorkerW is a direct child of Progman and does not
    // contain the icon view.
    for child in workerw_children(progman) {
        if first_child_of_class(child, CLASS_DEFVIEW).is_none() {
            return Some(child);
        }
    }

    // Classic layout: the DefView owner is a top-level WorkerW and the
    // wallpaper WorkerW is its next sibling in Z-order.
    let mut top: Vec<HWND> = Vec::new();
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the Vec passed by `find_wallpaper_workerw`.
        let list = unsafe { &mut *(lparam as *mut Vec<HWND>) };
        list.push(hwnd);
        1
    }
    // SAFETY: the callback only touches the Vec, which outlives the call.
    unsafe { EnumWindows(Some(collect), (&mut top as *mut Vec<HWND>) as LPARAM) };

    for owner in top {
        if first_child_of_class(owner, CLASS_DEFVIEW).is_none() {
            continue;
        }
        let mut sibling = unsafe { GetWindow(owner, GW_HWNDNEXT) };
        let mut hops = 0;
        while !sibling.is_null() && hops < 8 {
            if class_name(sibling) == CLASS_WORKERW {
                return Some(sibling);
            }
            // SAFETY: `sibling` is a valid window from GetWindow.
            sibling = unsafe { GetWindow(sibling, GW_HWNDNEXT) };
            hops += 1;
        }
    }

    None
}

/// Decide which host strategy this Explorer build needs.
///
/// `WS_EX_NOREDIRECTIONBITMAP` on `Progman` is the documented marker for the
/// raised desktop (build 26100+); a layered `DefView` child confirms it.
fn desktop_mode(progman: HWND, view: HWND) -> DesktopMode {
    let progman_raised = ex_style(progman) & WS_EX_NOREDIRECTIONBITMAP != 0;
    let view_layered = !view.is_null() && ex_style(view) & WS_EX_LAYERED != 0;
    if progman_raised || view_layered {
        DesktopMode::LayeredProgmanChild
    } else {
        DesktopMode::WorkerWChild
    }
}

/// Wait for Explorer's desktop and resolve the wallpaper surface.
///
/// The `Run` key can fire before Explorer has created `Progman`, so this polls
/// with backoff instead of sleeping for a fixed duration.
pub fn discover(timeout: Duration) -> Result<ShellSurface, ShellError> {
    let started = Instant::now();
    let mut backoff = Duration::from_millis(100);

    loop {
        if let Some(progman) = progman() {
            ask_explorer_to_spawn_worker(progman, 0x0D, 1);
            let mut workerw = find_wallpaper_workerw(progman);
            if workerw.is_none() {
                // Some builds only answer the second variant.
                ask_explorer_to_spawn_worker(progman, 0, 0);
                workerw = find_wallpaper_workerw(progman);
            }
            if let Some(workerw) = workerw {
                let view = defview(progman).unwrap_or(std::ptr::null_mut());
                return Ok(ShellSurface {
                    progman,
                    workerw,
                    defview: view,
                    mode: desktop_mode(progman, view),
                });
            }
            if started.elapsed() >= timeout {
                return Err(ShellError::NoWallpaperSurface);
            }
        } else if started.elapsed() >= timeout {
            return Err(ShellError::ProgmanTimeout {
                waited_ms: started.elapsed().as_millis(),
            });
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

/// True when every window we depend on still exists.
pub fn surface_is_alive(surface: &ShellSurface) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
    // SAFETY: handles are only compared; IsWindow tolerates stale handles.
    unsafe {
        IsWindow(surface.progman) != 0
            && IsWindow(surface.workerw) != 0
            && (surface.defview.is_null() || IsWindow(surface.defview) != 0)
    }
}

/// True when `upper` is above `lower` in the Z-order of their shared parent.
pub fn is_above(upper: HWND, lower: HWND) -> bool {
    if upper.is_null() || lower.is_null() {
        return false;
    }
    // SAFETY: both handles are only inspected; GW_CHILD returns the topmost
    // child and GW_HWNDNEXT walks down the Z-order.
    unsafe {
        let mut current = GetWindow(upper, GW_HWNDNEXT);
        let mut hops = 0;
        while !current.is_null() && hops < 64 {
            if current == lower {
                return true;
            }
            current = GetWindow(current, GW_HWNDNEXT);
            hops += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_mode_labels_are_stable() {
        // `doctor` prints these labels; keep them stable.
        assert_eq!(DesktopMode::WorkerWChild.label(), "workerw_child");
        assert_eq!(
            DesktopMode::LayeredProgmanChild.label(),
            "layered_progman_child"
        );
    }

    #[test]
    fn host_parent_follows_mode() {
        let progman = 0x111usize as HWND;
        let workerw = 0x222usize as HWND;

        let classic = ShellSurface {
            progman,
            workerw,
            defview: std::ptr::null_mut(),
            mode: DesktopMode::WorkerWChild,
        };
        assert_eq!(classic.host_parent(), workerw);

        let raised = ShellSurface {
            mode: DesktopMode::LayeredProgmanChild,
            ..classic
        };
        assert_eq!(raised.host_parent(), progman);
    }

    #[test]
    fn null_handles_report_not_above() {
        assert!(!is_above(std::ptr::null_mut(), std::ptr::null_mut()));
        assert!(!is_above(0x111usize as HWND, std::ptr::null_mut()));
    }

    #[test]
    fn shell_error_messages_are_actionable() {
        assert_eq!(
            ShellError::ProgmanTimeout { waited_ms: 60_000 }.to_string(),
            "Explorer's Progman window did not appear within 60000 ms"
        );
        assert!(ShellError::NoWallpaperSurface
            .to_string()
            .contains("no wallpaper WorkerW"));
    }
}
