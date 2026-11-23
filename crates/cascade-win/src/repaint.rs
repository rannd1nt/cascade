//! The repaint ladder.
//!
//! Cascade does not own the wallpaper setting, so "restore" means "make
//! Explorer paint the desktop again". Levels run in order; the first successful
//! level stops the ladder, except that L1 always runs.
//!
//! See for the rationale and for the open question of
//! which level is actually sufficient on a given build.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, SystemParametersInfoW, SMTO_ABORTIFHUNG, SMTO_NORMAL, SPIF_SENDCHANGE,
    SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
};

use crate::shell::ShellSurface;
use crate::wallpaper::WallpaperSnapshot;
use crate::wide;

const SPAWN_WORKER_MESSAGE: u32 = 0x052C;

#[derive(Clone, Debug)]
pub struct RepaintAttempt {
    pub level: u32,
    pub action: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct RepaintReport {
    pub attempts: Vec<RepaintAttempt>,
}

impl RepaintReport {
    pub fn highest_success(&self) -> Option<u32> {
        self.attempts.iter().rev().find(|a| a.ok).map(|a| a.level)
    }

    pub fn any_success(&self) -> bool {
        self.attempts.iter().any(|a| a.ok)
    }
}

fn redraw(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let flags = RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW;
    // SAFETY: `hwnd` is a valid window or null (checked above); null update
    // rect and region are documented as "whole window".
    unsafe { RedrawWindow(hwnd, std::ptr::null(), std::ptr::null_mut(), flags) != 0 }
}

/// Run the ladder up to `max_level` (1..=4).
pub fn ladder(
    surface: &ShellSurface,
    snapshot: &WallpaperSnapshot,
    max_level: u32,
) -> RepaintReport {
    let mut report = RepaintReport::default();
    let max_level = max_level.clamp(1, 4);

    // L1 — destroy our windows (done by the caller) and repaint the surface.
    let worker_ok = redraw(surface.workerw);
    let progman_ok = redraw(surface.progman);
    report.attempts.push(RepaintAttempt {
        level: 1,
        action: "RedrawWindow(wallpaper surface)",
        ok: worker_ok || progman_ok,
        detail: format!("workerw={worker_ok} progman={progman_ok}"),
    });
    if max_level == 1 {
        return report;
    }

    // L2 — ask Explorer to rebuild the wallpaper surface.
    let mut result = 0usize;
    // SAFETY: valid handle; timeout prevents a hung Explorer from blocking exit.
    let sent = unsafe {
        SendMessageTimeoutW(
            surface.progman,
            SPAWN_WORKER_MESSAGE,
            0x0D,
            1,
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            1_000,
            &mut result,
        )
    };
    report.attempts.push(RepaintAttempt {
        level: 2,
        action: "SendMessageTimeout(Progman, 0x052C)",
        ok: sent != 0,
        detail: format!("result={result}"),
    });
    if max_level == 2 {
        return report;
    }

    // L3 — re-apply the current setting without changing it. On Windows 11 a
    // NULL path is commonly a no-op; the boolean only reports acceptance.
    // SAFETY: null data pointer is explicitly allowed for this action.
    let l3 = unsafe { SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, std::ptr::null_mut(), 0) };
    report.attempts.push(RepaintAttempt {
        level: 3,
        action: "SystemParametersInfo(SPI_SETDESKWALLPAPER, NULL)",
        ok: l3 != 0,
        detail: if l3 != 0 {
            "accepted".to_string()
        } else {
            "rejected or no-op".to_string()
        },
    });
    if max_level == 3 {
        return report;
    }

    // L4 — last resort: write the snapshotted path back, but only when the
    // wallpaper is still a plain picture and the user has not changed it since
    // the snapshot. Slideshow / Spotlight / solid colour are never rewritten.
    let still_plain_picture = snapshot.background_type == Some(0);
    let (ok, detail) = match snapshot.path.as_deref() {
        Some(path) if still_plain_picture && !crate::wallpaper::changed_since(snapshot) => {
            let mut path_w = wide::to_wide(path);
            // SAFETY: `path_w` is a valid NUL-terminated buffer that lives for
            // the duration of the call.
            let r = unsafe {
                SystemParametersInfoW(
                    SPI_SETDESKWALLPAPER,
                    0,
                    path_w.as_mut_ptr().cast(),
                    SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
                )
            };
            (r != 0, format!("re-applied {path}"))
        }
        Some(_) if !still_plain_picture => (
            false,
            "skipped: wallpaper is not a plain picture".to_string(),
        ),
        Some(_) => (
            false,
            "skipped: wallpaper changed since snapshot".to_string(),
        ),
        None => (false, "skipped: no wallpaper path in snapshot".to_string()),
    };
    report.attempts.push(RepaintAttempt {
        level: 4,
        action: "SystemParametersInfo(SPI_SETDESKWALLPAPER, snapshot)",
        ok,
        detail,
    });

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(level: u32, ok: bool) -> RepaintAttempt {
        RepaintAttempt {
            level,
            action: "test",
            ok,
            detail: String::new(),
        }
    }

    #[test]
    fn empty_report_has_no_success() {
        let report = RepaintReport::default();
        assert!(!report.any_success());
        assert_eq!(report.highest_success(), None);
    }

    #[test]
    fn highest_success_is_the_last_ok_level() {
        // Attempts are pushed in ascending level order, so "last ok" is the
        // highest level that actually ran.
        let mut report = RepaintReport::default();
        report.attempts.push(attempt(1, true));
        report.attempts.push(attempt(2, false));
        assert!(report.any_success());
        assert_eq!(report.highest_success(), Some(1));

        report.attempts.push(attempt(3, true));
        assert_eq!(report.highest_success(), Some(3));
    }

    #[test]
    fn only_failures_report_no_success() {
        let mut report = RepaintReport::default();
        report.attempts.push(attempt(1, false));
        report.attempts.push(attempt(2, false));
        assert!(!report.any_success());
        assert_eq!(report.highest_success(), None);
    }
}
