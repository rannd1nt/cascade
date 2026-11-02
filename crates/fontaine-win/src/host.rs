//! Desktop host windows.
//!
//! One child window per monitor. On the classic desktop they are children of
//! the wallpaper `WorkerW`; on Windows 11 24H2+ they are layered children of
//! `Progman`, z-ordered between `SHELLDLL_DefView` and the wallpaper `WorkerW`.
//!
//! Slice 1 paints a solid colour; the media backend replaces the painted
//! content in M2 without changing this contract.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use fontaine_core::Rect;
use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, MapWindowPoints, UpdateWindow,
    HDC, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClassNameW, GetClientRect, GetParent,
    GetWindowLongPtrW, GetWindowRect, IsWindow, IsWindowVisible, LoadCursorW, RegisterClassExW,
    SetLayeredWindowAttributes, SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWLP_USERDATA, GWL_STYLE, HWND_BOTTOM, HWND_TOP, IDC_ARROW, LWA_ALPHA, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE,
    WM_ENDSESSION, WM_ERASEBKGND, WM_NCDESTROY, WM_PAINT, WM_QUERYENDSESSION, WNDCLASSEXW,
    WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

use crate::monitor::MonitorInfo;
use crate::present::Presenter;
use crate::shell::{self, DesktopMode, ShellSurface};
use crate::wide;

pub const HOST_CLASS: &str = "FontaineHost";

/// The painted background. `COLORREF` is `0x00BBGGRR`.
pub fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// `COLORREF` to `RRGGBB`, for logs and `doctor`.
pub fn rgb_hex(color: COLORREF) -> String {
    format!(
        "{:02X}{:02X}{:02X}",
        color & 0xFF,
        (color >> 8) & 0xFF,
        (color >> 16) & 0xFF
    )
}

/// `COLORREF` to the sRGB clear value a `B8G8R8A8_UNORM` target expects.
fn color_rgba(color: COLORREF) -> [f32; 4] {
    [
        (color & 0xFF) as f32 / 255.0,
        ((color >> 8) & 0xFF) as f32 / 255.0,
        ((color >> 16) & 0xFF) as f32 / 255.0,
        1.0,
    ]
}

/// Which desktop layout to attach to.
///
/// `Auto` follows what `shell::discover` detected. The other two exist so the
/// layout can be forced for diagnostics and for experiments on new builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostModePreference {
    Auto,
    WorkerW,
    Layered,
}

impl HostModePreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::WorkerW => "workerw",
            Self::Layered => "layered",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(Self::Auto),
            "workerw" => Some(Self::WorkerW),
            "layered" => Some(Self::Layered),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HostOptions {
    pub color: COLORREF,
    pub mode: HostModePreference,
}

impl Default for HostOptions {
    fn default() -> Self {
        // Teal: unmistakably not a desktop wallpaper, easy to verify in a
        // screenshot without being painful to look at.
        Self {
            color: rgb(0x18, 0x7A, 0x86),
            mode: HostModePreference::Auto,
        }
    }
}

#[derive(Debug)]
pub enum HostError {
    ClassRegistration(u32),
    CreateWindow { monitor: String, code: u32 },
    Layered { monitor: String, code: u32 },
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClassRegistration(code) => {
                write!(f, "could not register the host window class (error {code})")
            }
            Self::CreateWindow { monitor, code } => {
                write!(
                    f,
                    "could not create the host window for {monitor} (error {code})"
                )
            }
            Self::Layered { monitor, code } => write!(
                f,
                "could not make the host window for {monitor} layered (error {code})"
            ),
        }
    }
}

impl std::error::Error for HostError {}

struct HostState {
    color: COLORREF,
    /// True when a swap chain drives this window, so `WM_PAINT` must not
    /// paint over it with GDI.
    presented: bool,
}

/// Emergency-visible list of live host windows, used by the shutdown/panic
/// paths where we cannot borrow the `Host` value.
static LIVE_WINDOWS: Mutex<Vec<isize>> = Mutex::new(Vec::new());
static LIVE_SURFACE: Mutex<Option<SurfaceHandles>> = Mutex::new(None);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// `ShellSurface` holds raw pointers and is not `Send`; flatten it for the
/// process-wide emergency registry.
#[derive(Clone, Copy)]
struct SurfaceHandles {
    progman: isize,
    workerw: isize,
    defview: isize,
    mode: DesktopMode,
}

impl SurfaceHandles {
    fn from(surface: &ShellSurface) -> Self {
        Self {
            progman: surface.progman as isize,
            workerw: surface.workerw as isize,
            defview: surface.defview as isize,
            mode: surface.mode,
        }
    }

    fn to_surface(self) -> ShellSurface {
        ShellSurface {
            progman: self.progman as HWND,
            workerw: self.workerw as HWND,
            defview: self.defview as HWND,
            mode: self.mode,
        }
    }
}

pub struct HostWindow {
    pub hwnd: HWND,
    pub monitor: MonitorInfo,
    /// `Some` when the window is drawn by a DXGI swap chain. Must be dropped
    /// before the HWND, otherwise DXGI keeps a reference to a dead window.
    presenter: Option<Presenter>,
}

pub struct Host {
    windows: Vec<HostWindow>,
    surface: ShellSurface,
}

fn ensure_class() -> Result<(), HostError> {
    if CLASS_REGISTERED.load(Ordering::Relaxed) {
        return Ok(());
    }
    // SAFETY: null requests the module handle of the running executable.
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = wide::to_wide(HOST_CLASS);
    // SAFETY: IDC_ARROW is a system resource identifier.
    let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(host_wndproc),
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

    // SAFETY: `wc` is fully initialized and points to live strings for the call.
    let atom = unsafe { RegisterClassExW(&wc) };
    let code = if atom == 0 {
        // SAFETY: no pointers; reads the thread's last-error value.
        unsafe { GetLastError() }
    } else {
        0
    };
    if atom == 0 {
        const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
        if code != ERROR_CLASS_ALREADY_EXISTS {
            return Err(HostError::ClassRegistration(code));
        }
    }
    CLASS_REGISTERED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Paint the whole client area with the host colour.
///
/// # Safety
/// `hwnd` must be a host window and `hdc` a valid device context.
unsafe fn paint(hwnd: HWND, hdc: HDC) {
    // SAFETY: GWLP_USERDATA holds a `*mut HostState` for host windows.
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const HostState;
    let color = if state.is_null() {
        rgb(0, 0, 0)
    } else {
        // SAFETY: pointer was created from `Box::into_raw` and lives until
        // WM_NCDESTROY.
        let state = unsafe { &*state };
        if state.presented {
            // The swap chain owns the pixels; GDI would fight it.
            return;
        }
        state.color
    };

    // SAFETY: zeroed RECT is valid; GetClientRect fills it.
    let mut rc: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: valid window handle and out pointer.
    if unsafe { GetClientRect(hwnd, &mut rc) } == 0 {
        return;
    }
    // SAFETY: creating a GDI brush has no preconditions.
    let brush = unsafe { CreateSolidBrush(color) };
    if brush.is_null() {
        return;
    }
    // SAFETY: `hdc` is valid, `rc` is a valid rectangle, `brush` is a valid brush.
    unsafe {
        FillRect(hdc, &rc, brush);
        DeleteObject(brush);
    }
}

unsafe extern "system" fn host_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => {
            // SAFETY: wParam carries the device context for this message.
            unsafe { paint(hwnd, wparam as HDC) };
            1
        }
        WM_PAINT => {
            // SAFETY: zeroed PAINTSTRUCT is valid; BeginPaint fills it.
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            // SAFETY: valid window handle and out pointer.
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !hdc.is_null() {
                // SAFETY: hdc is valid until EndPaint.
                unsafe { paint(hwnd, hdc) };
            }
            // SAFETY: `ps` was filled by BeginPaint.
            unsafe { EndPaint(hwnd, &ps) };
            0
        }
        WM_NCDESTROY => {
            // SAFETY: GWLP_USERDATA may hold the HostState allocated on attach.
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut HostState;
            if !ptr.is_null() {
                // SAFETY: clear first so a re-entrant message cannot double-free.
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                // SAFETY: pointer came from Box::into_raw exactly once.
                unsafe { drop(Box::from_raw(ptr)) };
            }
            // SAFETY: default handling for window destruction.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // Never delay a shutdown or logoff; we have no unsaved data.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => 0,
        _ => {
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Create one host window for `monitor`, ready to be shown.
///
/// On the raised desktop the window has to be created as a *top-level*
/// layered window and converted into a `Progman` child afterwards. Creating it
/// as a `WS_CHILD` of `Progman` directly fails inside `CreateWindowExW`: the
/// call returns NULL and does not even set a last-error code (the window is
/// destroyed again during creation). The create-then-reparent sequence is the
/// one the shell accepts, and it is also what other live-wallpaper engines do.
fn create_window(surface: &ShellSurface, monitor: &MonitorInfo) -> Result<HWND, HostError> {
    // SAFETY: null requests the module handle of the running executable.
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = wide::to_wide(HOST_CLASS);
    let title = wide::to_wide("Fontaine wallpaper");
    let (x, y, w, h) = monitor_rect_in_parent(surface, monitor.rect);
    let layered = surface.mode == DesktopMode::LayeredProgmanChild;

    let (ex_style, style, parent) = if layered {
        (
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            WS_POPUP,
            std::ptr::null_mut(),
        )
    } else {
        (
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            WS_CHILD | WS_VISIBLE,
            surface.host_parent(),
        )
    };

    // SAFETY: class/title are NUL-terminated and live for the call; `parent`
    // is either a live desktop window or the desktop itself.
    let hwnd = unsafe {
        SetLastError(0);
        CreateWindowExW(
            ex_style,
            class.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            w,
            h,
            parent,
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        // SAFETY: reads the thread's last-error value.
        let code = unsafe { GetLastError() };
        return Err(HostError::CreateWindow {
            monitor: monitor.id.clone(),
            code,
        });
    }

    if layered {
        // A fully opaque layered window is what the raised desktop expects.
        // SAFETY: valid window handle; alpha 255 keeps it opaque.
        if unsafe { SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA) } == 0 {
            // SAFETY: reads the thread's last-error value.
            let code = unsafe { GetLastError() };
            // SAFETY: handle was created above.
            unsafe { DestroyWindow(hwnd) };
            return Err(HostError::Layered {
                monitor: monitor.id.clone(),
                code,
            });
        }
        // SAFETY: both handles are live; SetParent keeps the window alive and
        // the style edit turns the popup into a real child of `Progman`.
        unsafe {
            SetParent(hwnd, surface.progman);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            SetWindowLongPtrW(hwnd, GWL_STYLE, ((style | WS_CHILD) & !WS_POPUP) as isize);
            // SetParent reinterprets the position relative to the new parent,
            // so place it again with parent-relative coordinates.
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }

    Ok(hwnd)
}

fn monitor_rect_in_parent(surface: &ShellSurface, rect: Rect) -> (i32, i32, i32, i32) {
    let mut points = [
        POINT {
            x: rect.x,
            y: rect.y,
        },
        POINT {
            x: rect.x + rect.w,
            y: rect.y + rect.h,
        },
    ];
    // SAFETY: HWND_DESKTOP (null) means screen coordinates; the parent is a
    // live window and `points` has exactly two entries.
    unsafe {
        MapWindowPoints(
            std::ptr::null_mut(),
            surface.host_parent(),
            points.as_mut_ptr(),
            2,
        )
    };
    (
        points[0].x,
        points[0].y,
        points[1].x - points[0].x,
        points[1].y - points[0].y,
    )
}

/// Put the host windows between the icons and the wallpaper.
///
/// On the raised desktop the system wallpaper is a sibling `WorkerW` *above*
/// any child window, so creating the window is not enough: the z-order has to
/// be fixed explicitly. `SetWindowPos(a, b, ..)` places `a` directly behind
/// `b`, which is why the wallpaper is moved behind the lowest host window.
fn restack(surface: &ShellSurface, windows: &[HostWindow]) {
    if surface.mode != DesktopMode::LayeredProgmanChild || windows.is_empty() {
        return;
    }
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;

    if surface.defview.is_null() {
        for window in windows {
            // SAFETY: handles are live; only the Z-order is touched.
            unsafe { SetWindowPos(window.hwnd, HWND_TOP, 0, 0, 0, 0, flags) };
        }
        // SAFETY: same as above; keep the system wallpaper behind everything.
        unsafe { SetWindowPos(surface.workerw, HWND_BOTTOM, 0, 0, 0, 0, flags) };
        return;
    }

    // Inserting in reverse leaves the windows in creation order under DefView.
    for window in windows.iter().rev() {
        // SAFETY: handles are live; only the Z-order is touched.
        unsafe { SetWindowPos(window.hwnd, surface.defview, 0, 0, 0, 0, flags) };
    }
    if let Some(lowest) = windows.last() {
        // SAFETY: handles are live; only the Z-order is touched.
        unsafe { SetWindowPos(surface.workerw, lowest.hwnd, 0, 0, 0, 0, flags) };
    }
}

fn register_live(windows: &[HostWindow], surface: &ShellSurface) {
    if let Ok(mut live) = LIVE_WINDOWS.lock() {
        live.clear();
        live.extend(windows.iter().map(|w| w.hwnd as isize));
    }
    if let Ok(mut s) = LIVE_SURFACE.lock() {
        *s = Some(SurfaceHandles::from(surface));
    }
}

fn unregister_live() {
    if let Ok(mut live) = LIVE_WINDOWS.lock() {
        live.clear();
    }
    if let Ok(mut s) = LIVE_SURFACE.lock() {
        *s = None;
    }
}

impl Host {
    /// Create and show one host window per monitor.
    pub fn attach(
        mut surface: ShellSurface,
        monitors: &[MonitorInfo],
        opts: &HostOptions,
    ) -> Result<Self, HostError> {
        ensure_class()?;
        surface.mode = match opts.mode {
            HostModePreference::Auto => surface.mode,
            HostModePreference::WorkerW => DesktopMode::WorkerWChild,
            HostModePreference::Layered => DesktopMode::LayeredProgmanChild,
        };
        let mut windows: Vec<HostWindow> = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            match create_window(&surface, monitor) {
                Ok(hwnd) => {
                    let presenter = match Presenter::new(
                        hwnd as isize,
                        monitor.rect.w.max(1) as u32,
                        monitor.rect.h.max(1) as u32,
                    ) {
                        Ok(presenter) => Some(presenter),
                        Err(err) => {
                            // Falling back to GDI keeps the classic desktop
                            // working; on the raised desktop it will not be
                            // visible, so say so loudly.
                            eprintln!("host: {} presenter unavailable: {err}", monitor.id);
                            None
                        }
                    };
                    let state = Box::into_raw(Box::new(HostState {
                        color: opts.color,
                        presented: presenter.is_some(),
                    }));
                    // SAFETY: the pointer is owned by the window until
                    // WM_NCDESTROY.
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
                    // SAFETY: show without activating; never steal focus.
                    unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
                    // SAFETY: forces the first WM_PAINT before we report success.
                    unsafe { UpdateWindow(hwnd) };
                    if let Some(presenter) = &presenter {
                        presenter.clear(color_rgba(opts.color));
                    }

                    windows.push(HostWindow {
                        hwnd,
                        monitor: monitor.clone(),
                        presenter,
                    });
                }
                Err(err) => {
                    for created in &windows {
                        // SAFETY: handles were created above and are still valid.
                        unsafe { DestroyWindow(created.hwnd) };
                    }
                    unregister_live();
                    return Err(err);
                }
            }
        }

        restack(&surface, &windows);
        register_live(&windows, &surface);
        Ok(Self { windows, surface })
    }

    pub fn surface(&self) -> &ShellSurface {
        &self.surface
    }

    pub fn windows(&self) -> &[HostWindow] {
        &self.windows
    }

    /// Re-apply the Z-order. Explorer or another wallpaper app can move the
    /// system wallpaper above us; the watchdog calls this when it detects it.
    pub fn restack(&self) {
        restack(&self.surface, &self.windows);
    }

    /// Destroy every host window. The caller must run the repaint ladder next.
    pub fn detach(&mut self) {
        for mut window in self.windows.drain(..) {
            // Drop the swap chain while the HWND is still alive.
            window.presenter = None;
            // SAFETY: handle was created by this process; stale handles are
            // tolerated by DestroyWindow.
            unsafe { DestroyWindow(window.hwnd) };
        }
        unregister_live();
    }

    /// Cheap health check used by the watchdog in a later slice.
    pub fn is_healthy(&self) -> bool {
        if !shell::surface_is_alive(&self.surface) {
            return false;
        }
        let parent = self.surface.host_parent();
        self.windows.iter().all(|window| {
            // SAFETY: all functions below only inspect window state.
            unsafe {
                IsWindow(window.hwnd) != 0
                    && IsWindowVisible(window.hwnd) != 0
                    && GetParent(window.hwnd) == parent
                    && rect_matches(window.hwnd, window.monitor.rect)
                    && (self.surface.mode != DesktopMode::LayeredProgmanChild
                        || shell::is_above(window.hwnd, self.surface.workerw))
            }
        })
    }

    /// Human-readable snapshot of the window hierarchy, for `doctor` and logs.
    pub fn describe(&self) -> Vec<String> {
        self.windows
            .iter()
            .map(|w| {
                let mut buf = [0u16; 256];
                // SAFETY: valid handle and buffer.
                let len = unsafe { GetClassNameW(w.hwnd, buf.as_mut_ptr(), buf.len() as i32) };
                let class = if len > 0 {
                    wide::from_wide(&buf[..len as usize])
                } else {
                    "?".to_string()
                };
                // SAFETY: GetParent only inspects the handle.
                let parent = unsafe { GetParent(w.hwnd) };
                let above = shell::is_above(w.hwnd, self.surface.workerw);
                let present = match &w.presenter {
                    Some(presenter) => format!("dxgi/{}", presenter.effect().label()),
                    None => "gdi".to_string(),
                };
                format!(
                    "{} host=0x{:x} class={class} parent=0x{:x} above_wallpaper={above} present={present}",
                    w.monitor.id, w.hwnd as usize, parent as usize
                )
            })
            .collect()
    }
}

fn rect_matches(hwnd: HWND, expected: Rect) -> bool {
    // SAFETY: zeroed RECT is valid; GetWindowRect fills it for valid windows.
    let mut rc: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: valid handle and out pointer.
    if unsafe { GetWindowRect(hwnd, &mut rc) } == 0 {
        return false;
    }
    rc.left == expected.x
        && rc.top == expected.y
        && (rc.right - rc.left) == expected.w
        && (rc.bottom - rc.top) == expected.h
}

/// Emergency detach used by the console handler, session end, and panic hook.
///
/// Destroys any host window still registered, then runs the repaint ladder.
/// Returns false when another thread holds the registry lock (treated as
/// "nothing to do"; the OS will destroy the windows with the process anyway).
pub fn emergency_cleanup(max_level: u32) -> bool {
    let handles: Vec<isize> = match LIVE_WINDOWS.try_lock() {
        Ok(mut live) => std::mem::take(&mut *live),
        Err(_) => return false,
    };
    let surface = LIVE_SURFACE.try_lock().ok().and_then(|mut s| s.take());
    if let Ok(mut live) = LIVE_WINDOWS.try_lock() {
        live.clear();
    }

    for handle in handles {
        let hwnd = handle as HWND;
        // SAFETY: the handle was created by this process; IsWindow guards reuse.
        unsafe {
            if IsWindow(hwnd) != 0 {
                DestroyWindow(hwnd);
            }
        }
    }

    if let Some(surface) = surface {
        let surface = surface.to_surface();
        let snapshot = crate::wallpaper::snapshot();
        let _ = crate::repaint::ladder(&surface, &snapshot, max_level);
    }
    true
}

/// True when this process currently has host windows registered.
pub fn has_live_windows() -> bool {
    LIVE_WINDOWS
        .try_lock()
        .map(|live| !live.is_empty())
        .unwrap_or(false)
}
