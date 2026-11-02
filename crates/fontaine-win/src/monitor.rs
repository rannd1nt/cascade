//! Monitor enumeration.

use fontaine_core::Rect;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use crate::wide;

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    /// `HMONITOR` as an integer so the struct stays `Send`.
    pub handle: isize,
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub id: String,
    pub rect: Rect,
    pub work: Rect,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn width(&self) -> i32 {
        self.rect.w
    }

    pub fn height(&self) -> i32 {
        self.rect.h
    }
}

fn to_rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<MonitorInfo>` passed by `enumerate`.
    let out = unsafe { &mut *(lparam as *mut Vec<MonitorInfo>) };

    // SAFETY: zeroed MONITORINFOEXW is a valid value; cbSize is set below.
    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    // SAFETY: MONITORINFOEXW starts with a MONITORINFO, so the cast is valid,
    // and `hmon` comes from the enumeration callback.
    let ok = unsafe {
        GetMonitorInfoW(
            hmon,
            (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>(),
        )
    };
    if ok != 0 {
        out.push(MonitorInfo {
            handle: hmon as isize,
            id: wide::from_wide(&info.szDevice),
            rect: to_rect(info.monitorInfo.rcMonitor),
            work: to_rect(info.monitorInfo.rcWork),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    1
}

/// Enumerate all monitors, primary first.
pub fn enumerate() -> Vec<MonitorInfo> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    let ptr = (&mut monitors as *mut Vec<MonitorInfo>) as LPARAM;
    // SAFETY: the callback only writes to `monitors`, which outlives the call.
    unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(enum_proc), ptr);
    }
    monitors.sort_by_key(|m| (!m.primary, m.rect.x, m.rect.y));
    monitors
}
