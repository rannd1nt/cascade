//! Read-only snapshot of the user's wallpaper configuration.
//!
//! Fontaine never installs itself as the wallpaper. This snapshot exists for
//! diagnostics and for the last-resort repaint level (re-applying the *current*
//! setting without changing it).

use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;

use crate::registry;

const DESKTOP_KEY: &str = r"Control Panel\Desktop";
const WALLPAPERS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Wallpapers";

#[derive(Clone, Debug, Default)]
pub struct WallpaperSnapshot {
    pub path: Option<String>,
    /// `Explorer\Wallpapers\BackgroundType`: 0 = picture, 1 = managed by the
    /// Photos app / Spotlight, 2 = solid colour, 3 = slideshow (observations
    /// vary; treat as opaque diagnostic data).
    pub background_type: Option<u32>,
    /// Where the path came from, for `doctor` output.
    pub source: &'static str,
}

pub fn snapshot() -> WallpaperSnapshot {
    let current = registry::read_string(HKEY_CURRENT_USER, WALLPAPERS_KEY, "CurrentWallpaperPath")
        .filter(|s| !s.is_empty());
    let legacy = registry::read_string(HKEY_CURRENT_USER, DESKTOP_KEY, "WallPaper")
        .filter(|s| !s.is_empty());

    let (path, source) = match (current, legacy) {
        (Some(p), _) => (Some(p), "Explorer\\Wallpapers"),
        (None, Some(p)) => (Some(p), "Control Panel\\Desktop"),
        (None, None) => (None, "none"),
    };

    WallpaperSnapshot {
        path,
        background_type: registry::read_u32(HKEY_CURRENT_USER, WALLPAPERS_KEY, "BackgroundType"),
        source,
    }
}

/// True when the wallpaper path currently configured differs from `snapshot`.
pub fn changed_since(snapshot: &WallpaperSnapshot) -> bool {
    let now = self::snapshot();
    now.path != snapshot.path || now.background_type != snapshot.background_type
}
