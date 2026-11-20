//! The tray icon and its menu, inside the daemon (design-m5 §6, P3).
//!
//! The icon hangs off the **existing hidden top-level control window**
//! ([`crate::control`]) — not a message-only window: system broadcasts
//! (`TaskbarCreated`) are not delivered to message-only windows, and the
//! control window already exists for session-level messaging.
//!
//! Split of work, same rule as session lock and power broadcasts:
//!
//! * the window procedure only records flags ([`request_menu`],
//!   [`request_reload`]);
//! * the main loop calls [`Tray::step`] twice per pass — after the
//!   command drain (a live `settings set tray` applies immediately) and
//!   after the message drain (a click recorded during that drain);
//! * [`Tray::step`] may show the menu (`TrackPopupMenu`, modal — it runs
//!   on the main loop thread while it tracks) and returns the chosen
//!   [`TrayAction`] for the loop to perform.
//!
//! Nothing here is fatal: a failed `NIM_ADD` logs once and retries at
//! the ≥2 s loop cadence; the desktop must not depend on a
//! notification-area icon.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, LoadImageW, PostMessageW,
    RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, HICON, HMENU, IDI_APPLICATION,
    IMAGE_ICON, LR_DEFAULTSIZE, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_NULL,
};

use crate::control::WM_APP_TRAY;
use crate::wide;
use crate::{derr, dlog};

/// The one notification-area icon this daemon owns (`uID` in
/// `NOTIFYICONDATAW`); also the identifier `Shell_NotifyIconGetRect`
/// looks up during verification.
const TRAY_UID: u32 = 1;

/// Tooltip: short, no version noise (design-m5 §6 — the tray is silent).
const TRAY_TIP: &str = "Cascade";

/// Resource ID of `img/cascade.ico`, embedded by `crates/cascade/build.rs`
/// (`1 ICON ...`). MAKEINTRESOURCEW is an integer-encoded pointer — the
/// same encoding windows-sys uses for `IDI_APPLICATION`.
const ICON_RESOURCE_ID: u32 = 1;

/// Set by the window procedure when a tray click arrives; consumed by
/// [`Tray::step`].
static MENU_PENDING: AtomicBool = AtomicBool::new(false);

/// Set when the registered `TaskbarCreated` broadcast arrives (explorer
/// restarted and dropped our icon); consumed by [`Tray::sync`].
static RELOAD_PENDING: AtomicBool = AtomicBool::new(false);

/// The registered `TaskbarCreated` message id; `0` until
/// [`Tray::new`] registers it (a guard comparing against `0` never
/// matches — registered ids are ≥ `0xC000`).
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Record a tray click from the window procedure; the main loop shows
/// the menu on its next turn (design-m5 §6 — wndproc = flags only).
pub fn request_menu() {
    MENU_PENDING.store(true, Ordering::Release);
}

/// Record a `TaskbarCreated` broadcast: explorer restarted, the shell
/// dropped our icon, the main loop re-adds it.
pub fn request_reload() {
    RELOAD_PENDING.store(true, Ordering::Release);
}

/// The registered `TaskbarCreated` id (0 = not registered yet); read by
/// the control window's procedure as a match guard.
pub fn taskbar_created_id() -> u32 {
    TASKBAR_CREATED.load(Ordering::Acquire)
}

/// Which menu this icon shows (`design-m5b.md` §4.7, D2). A process is
/// one role for its whole life, so the mode is fixed at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMode {
    /// The full daemon: Open / Pause / Quit (design-m5 §6).
    Full,
    /// The battery listener: only what it can honestly do. Pw2-S2 shows
    /// `Quit`; the disabled header and `Start now` (T6, MD1) arrive with
    /// Pw2-S3.
    Listening,
}

/// What one menu choice means for the main loop (pure; unit-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Spawn `current_exe() ui` — the same `cascade ui` command the CLI
    /// runs (AGENTS §1: the menu only opens existing CLI paths).
    OpenUi,
    /// Flip `flags.user_paused` and re-evaluate the pause oracle —
    /// identical to `cascade pause` / `cascade resume`.
    TogglePause,
    /// The shared quit function (design-m5 §4): the normal exit ladder,
    /// repaint first — never `process::exit` (AGENTS §13).
    Quit,
}

/// Menu entry ids; `TrackPopupMenu(TPM_RETURNCMD)` returns them verbatim.
const MENU_OPEN: u32 = 1;
const MENU_TOGGLE: u32 = 2;
const MENU_QUIT: u32 = 3;

/// Map a `TrackPopupMenu` command id to an action. Pure so the D8 menu
/// contract is unit-testable without a shell.
fn menu_action(cmd: u32) -> Option<TrayAction> {
    match cmd {
        MENU_OPEN => Some(TrayAction::OpenUi),
        MENU_TOGGLE => Some(TrayAction::TogglePause),
        MENU_QUIT => Some(TrayAction::Quit),
        // 0 = the menu was dismissed without a choice; anything else is
        // an id this build never created.
        _ => None,
    }
}

/// The pause entry shows the action it will perform (design-m5 D8).
fn pause_label(paused: bool) -> &'static str {
    if paused {
        "Resume"
    } else {
        "Pause"
    }
}

/// The daemon's tray state (design-m5 §6).
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    /// Which menu to build (Full vs Listening, `design-m5b.md` §4.7).
    mode: TrayMode,
    /// Whether the shell currently holds our icon — our own belief,
    /// updated only from real `NIM_ADD` / `NIM_DELETE` results.
    added: bool,
    /// An add failure was already logged for the current hidden epoch
    /// (retries keep happening; the log stays quiet).
    warned: bool,
}

impl Tray {
    /// Load the icon and apply `want` immediately. Registration of
    /// `TaskbarCreated` happens here, before the first `NIM_ADD`, so a
    /// broadcast can never arrive before we know its id.
    pub fn new(hwnd: HWND, want: bool, mode: TrayMode) -> Self {
        register_taskbar_created();
        let icon = load_icon();
        if icon.is_null() {
            derr!("tray: no icon resource; the tray entry would be text-only");
        }
        let mut tray = Self {
            hwnd,
            icon,
            mode,
            added: false,
            warned: false,
        };
        tray.sync(want);
        tray
    }

    /// One tray turn: reconcile the shell icon with `want` (the live
    /// `settings.tray`), then show the menu if the window procedure
    /// recorded a click since the last turn. Called twice per loop pass
    /// (design-m5 §6) — both call sites are cheap no-ops when nothing
    /// changed.
    pub fn step(&mut self, want: bool, paused: bool) -> Option<TrayAction> {
        self.sync(want);
        if !self.added || !MENU_PENDING.swap(false, Ordering::AcqRel) {
            return None;
        }
        show_menu(self.hwnd, self.mode, paused)
    }

    /// Drop the icon on the exit ladder (every path, including a
    /// respawn — the child re-adds its own): instant, and nothing is
    /// left for the shell to clean up after the window dies.
    pub fn remove(&mut self) {
        if self.added && notify(self.hwnd, self.icon, false) {
            dlog!("tray: icon removed");
        }
        self.added = false;
    }

    /// Reconcile the shell icon with `want`, consuming a pending
    /// explorer-restart reload first (the restart drops the icon while
    /// our HWND stays valid, so only an re-add is needed).
    fn sync(&mut self, want: bool) {
        if RELOAD_PENDING.swap(false, Ordering::AcqRel) && want {
            dlog!("tray: explorer restarted; re-adding the icon");
            self.added = false;
        }
        if want && !self.added {
            if notify(self.hwnd, self.icon, true) {
                self.added = true;
                self.warned = false;
                dlog!("tray: icon added");
            } else if !self.warned {
                // Retries continue at the loop cadence; only the first
                // failure of an epoch is logged.
                self.warned = true;
                derr!("tray: could not add the icon (notification area unavailable); retrying");
            }
        } else if !want && self.added {
            // A failed delete means explorer is gone — the icon went
            // with it either way, so the belief resets regardless.
            let _ = notify(self.hwnd, self.icon, false);
            self.added = false;
            dlog!("tray: icon removed");
        }
        if !want {
            // No icon, no click: never show a menu for a hidden tray.
            MENU_PENDING.store(false, Ordering::Release);
        }
    }
}

impl Drop for Tray {
    /// Safety net for early returns after [`Tray::new`] (the explicit
    /// [`Tray::remove`] on the exit ladder runs first for ordering);
    /// idempotent through the `added` flag.
    fn drop(&mut self) {
        if self.added {
            let _ = notify(self.hwnd, self.icon, false);
        }
    }
}

/// Add or delete the icon. `Shell_NotifyIconW` copies everything it
/// needs during the call; the struct is a plain `Default` (zeroed) value.
fn notify(hwnd: HWND, icon: HICON, add: bool) -> bool {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..NOTIFYICONDATAW::default()
    };
    if add {
        data.uFlags = NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        if !icon.is_null() {
            data.uFlags |= NIF_ICON;
            data.hIcon = icon;
        }
        // else: text-only entry — a missing icon resource must not
        // break the tray (the fallback in `load_icon` usually covers it).
        for (i, ch) in TRAY_TIP.encode_utf16().enumerate() {
            if i >= data.szTip.len() - 1 {
                break;
            }
            data.szTip[i] = ch;
        }
    }
    // SAFETY: `data` is fully initialized (Default zeroes every field
    // the shell reads), `hwnd` outlives this call, and `icon` was loaded
    // from this module's resources for the process lifetime.
    unsafe { Shell_NotifyIconW(if add { NIM_ADD } else { NIM_DELETE }, &data) != 0 }
}

/// Register the `TaskbarCreated` broadcast (same id for the process
/// lifetime; the control window's guard reads it through
/// [`taskbar_created_id`]).
fn register_taskbar_created() {
    let name = wide::to_wide("TaskbarCreated");
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let id = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if id == 0 {
        // Without the id no broadcast matches: the icon would not come
        // back after an explorer restart (the §12 scenario says it must).
        derr!("tray: could not register TaskbarCreated (explorer-restart re-add disabled)");
    }
    TASKBAR_CREATED.store(id, Ordering::Release);
}

/// The icon: resource 1 from this exe at the system icon size
/// (`LR_DEFAULTSIZE` → 32×32, the shell scales for DPI), falling back
/// to the stock application icon.
fn load_icon() -> HICON {
    // SAFETY: a valid module handle for the process lifetime.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: integer-encoded resource name (MAKEINTRESOURCEW), no
    // allocation; the loader returns a shared icon handle.
    let icon = unsafe {
        LoadImageW(
            module,
            ICON_RESOURCE_ID as _,
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE,
        )
    };
    if !icon.is_null() {
        return icon;
    }
    // SAFETY: the stock application icon is always available.
    unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) }
}

/// Build the menu at the cursor, track it modally on this thread, and
/// map the chosen id. Returns `None` on dismissal or a create failure.
///
/// Each mode offers only what that role can honestly do
/// (`design-m5b.md` §4.7): the listener has no engine, so `Open`/`Pause`
/// would be dead entries. Pw2-S2 ships `Quit` alone; the disabled header
/// and `Start now` (T6, MD1) join in Pw2-S3.
fn show_menu(hwnd: HWND, mode: TrayMode, paused: bool) -> Option<TrayAction> {
    // SAFETY: a valid menu handle when non-null.
    let menu: HMENU = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        derr!("tray: could not create the menu");
        return None;
    }
    let open = wide::to_wide("Open Cascade");
    let pause = wide::to_wide(pause_label(paused));
    let quit = wide::to_wide("Quit");
    // SAFETY: `menu` is live for this block, the labels are
    // NUL-terminated, and every out-parameter is a valid pointer.
    let cmd = unsafe {
        if mode == TrayMode::Full {
            AppendMenuW(menu, MF_STRING, MENU_OPEN as usize, open.as_ptr());
            AppendMenuW(menu, MF_STRING, MENU_TOGGLE as usize, pause.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        }
        AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, quit.as_ptr());

        let mut point = POINT::default();
        GetCursorPos(&mut point);
        // MSDN's tray-menu dismiss pattern: make this (hidden) window
        // foreground so a second click outside closes the menu, then
        // clear the activation the call caused with a no-op message.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        cmd
    };
    if cmd <= 0 {
        // Dismissed without a choice.
        return None;
    }
    menu_action(cmd as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_APP;

    #[test]
    fn menu_ids_map_to_the_three_d8_actions() {
        assert_eq!(menu_action(MENU_OPEN), Some(TrayAction::OpenUi));
        assert_eq!(menu_action(MENU_TOGGLE), Some(TrayAction::TogglePause));
        assert_eq!(menu_action(MENU_QUIT), Some(TrayAction::Quit));
        // 0 = dismissed; an id this build never created is ignored.
        assert_eq!(menu_action(0), None);
        assert_eq!(menu_action(99), None);
        // Distinct ids: `TrackPopupMenu` returns them verbatim.
        assert_ne!(MENU_OPEN, MENU_TOGGLE);
        assert_ne!(MENU_OPEN, MENU_QUIT);
        assert_ne!(MENU_TOGGLE, MENU_QUIT);
    }

    #[test]
    fn pause_menu_entry_shows_the_action_it_will_perform() {
        assert_eq!(pause_label(false), "Pause");
        assert_eq!(pause_label(true), "Resume");
    }

    #[test]
    fn tray_message_ids_stay_in_the_app_range_and_do_not_collide() {
        // The control window dispatches `WM_APP_TRAY` and the registered
        // `TaskbarCreated` id; both must be well-defined before any
        // icon exists (a stored `0` never matches a registered id).
        const { assert!(WM_APP_TRAY >= WM_APP, "must stay in the app range") };
        assert_ne!(WM_APP_TRAY, crate::control::WM_APP_PIPE);
        assert_ne!(WM_APP_TRAY, crate::control::WM_CASCADE_RESTORE);
        assert_eq!(taskbar_created_id(), 0, "unregistered until Tray::new");
    }
}
