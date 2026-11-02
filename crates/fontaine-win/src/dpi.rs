//! DPI awareness.
//!
//! This must run before any window or monitor coordinate is used. A mismatch
//! with the shell virtualizes coordinates and misplaces the host window.

use windows_sys::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Ask for per-monitor-v2 awareness for the process and the calling thread.
///
/// Fails silently when awareness was already set (for example when the binary
/// is launched by a host that set it first). Callers should not depend on the
/// return value.
pub fn set_per_monitor_v2() {
    // SAFETY: no pointers involved; failure is reported as a zero BOOL.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Current thread awareness as a human-readable label.
pub fn thread_awareness() -> &'static str {
    // SAFETY: both calls take and return borrowed context handles.
    let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
    match awareness {
        0 => "unaware",
        1 => "system-aware",
        2 => "per-monitor-aware",
        _ => "unknown",
    }
}
