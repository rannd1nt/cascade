//! Persistent settings (`%APPDATA%\Cascade\config.toml`, `architecture.md`
//! §10, design-m4 D10).
//!
//! The file holds the **file-backed** subset of the wire `Settings`: the
//! pause toggles and the power cluster. `autostart` is deliberately absent —
//! the Run key is the single oracle and `cascade autostart` writes it
//! directly, so a second copy in a file would only drift.
//!
//! Follows the `state.rs` pattern: best-effort, atomic (tmp + rename), a
//! corrupt or missing file degrades to the documented defaults and never
//! blocks the daemon. Read/write helpers take an explicit path so tests do
//! not touch the real profile.

use std::fs;
use std::path::{Path, PathBuf};

use cascade_core::ipc::Settings;
use cascade_core::rule_engine::{BatteryAction, PauseSettings, PowerProfile};
use cascade_core::PresentationMode;
use serde::{Deserialize, Serialize};

/// `config.toml` on disk. The table layout mirrors the documented example
/// in `architecture.md` §10; keep the names in sync when either changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub power: Power,
    pub pause: PauseSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Stored from S2 with no runtime consumer: the exit ladder always
    /// restores the desktop (AGENTS §3.1 outranks the flag).
    pub restore_wallpaper_on_exit: bool,
    /// Consumed by the M5 tray.
    pub tray: bool,
    /// Engine route the child builds at start (`design-engine-mode.md` §2):
    /// `accurate` (default) or `efficient`. Absent in an older file decodes
    /// to `accurate`, so upgrading never changes behaviour on its own.
    pub engine_mode: PresentationMode,
}

impl Default for General {
    fn default() -> Self {
        Self {
            restore_wallpaper_on_exit: true,
            tray: true,
            engine_mode: PresentationMode::Accurate,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Power {
    /// D8 default `off`.
    pub profile: PowerProfile,
    /// Absent in a hand-written file means "not set" → the D9 preset fills
    /// it from the profile on load. Writes from the daemon always emit the
    /// key, so an explicit `on_battery = "off"` is respected, not refilled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<BatteryAction>,
}

impl Default for Power {
    fn default() -> Self {
        Self {
            profile: PowerProfile::Off,
            on_battery: None,
        }
    }
}

/// `%APPDATA%\Cascade\config.toml`, or `None` when the environment is
/// unusable (the daemon then runs on defaults, best-effort like `state.rs`).
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("USERPROFILE")
            .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
    })?;
    Some(base.join("Cascade").join("config.toml"))
}

/// Load the persisted settings. A missing file is the normal first run and
/// yields the defaults; a corrupt one is logged and ignored (never blocks).
pub fn load() -> Settings {
    let Some(path) = path() else {
        return Settings::default();
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Settings::default(), // missing file: first run
    };
    read(&text)
}

/// Persist the settings atomically. The caller applies them in memory even
/// when this fails — a read-only profile directory must not undo a
/// `settings set`, it only costs durability (the failure is logged).
pub fn save(settings: &Settings) -> Result<(), String> {
    let path = path().ok_or_else(|| "no writable application data directory".to_string())?;
    write(&path, settings)
}

/// Decode settings from TOML text (missing tables/keys fall back to the
/// documented defaults through `#[serde(default)]`).
fn read(text: &str) -> Settings {
    match toml::from_str::<Config>(text) {
        Ok(config) => config.into_settings(),
        Err(err) => {
            crate::derr!("config: ignoring invalid config.toml: {err}");
            Settings::default()
        }
    }
}

/// Write settings to `path` atomically (tmp + rename, `state.rs` pattern).
fn write(path: &Path, settings: &Settings) -> Result<(), String> {
    let config = Config::from_settings(settings);
    let text = toml::to_string(&config).map_err(|err| format!("serialise config: {err}"))?;
    let Some(dir) = path.parent() else {
        return Err("config path has no parent".to_string());
    };
    fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|err| format!("write {}: {err}", tmp.display()))?;
    // `fs::rename` on Windows is MoveFileEx(REPLACE_EXISTING): atomic.
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("replace {}: {err}", path.display())
    })
}

impl Config {
    /// Wire view of the file. `autostart` is never stored, so it keeps the
    /// `Settings::default()` placeholder until the daemon fills it from the
    /// registry at the wire edge. A file without an `on_battery` key gets
    /// the D9 profile preset, exactly like `settings set power_profile`.
    pub fn into_settings(self) -> Settings {
        let preset = self.power.on_battery.is_none();
        let mut settings = Settings {
            power_profile: self.power.profile,
            pause: self.pause,
            on_battery: self.power.on_battery.unwrap_or(BatteryAction::Off),
            restore_wallpaper_on_exit: self.general.restore_wallpaper_on_exit,
            tray: self.general.tray,
            engine_mode: self.general.engine_mode,
            ..Settings::default()
        };
        if preset {
            settings.apply_profile_preset();
        }
        settings
    }

    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            general: General {
                restore_wallpaper_on_exit: settings.restore_wallpaper_on_exit,
                tray: settings.tray,
                engine_mode: settings.engine_mode,
            },
            power: Power {
                profile: settings.power_profile,
                on_battery: Some(settings.on_battery),
            },
            pause: settings.pause,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config file in a private temp directory, cleaned before use.
    fn temp_path(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("cascade-config-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (dir.clone(), dir.join("config.toml"))
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (dir, path) = temp_path("roundtrip");
        let mut settings = Settings::default();
        settings.pause.maximized = true;
        settings.power_profile = PowerProfile::Quality;
        settings.on_battery = BatteryAction::Continue;
        settings.tray = false;
        settings.engine_mode = PresentationMode::Efficient;
        write(&path, &settings).expect("write");
        let text = fs::read_to_string(&path).expect("read back");
        let loaded = read(&text);
        assert_eq!(loaded, settings, "settings survive a restart");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn autostart_is_never_written_to_the_file() {
        // The Run key is the oracle (design-m4 D10): a file copy would drift.
        let (dir, path) = temp_path("autostart");
        let settings = Settings {
            autostart: true,
            ..Settings::default()
        };
        write(&path, &settings).expect("write");
        let text = fs::read_to_string(&path).expect("read back");
        assert!(!text.contains("autostart"), "no autostart key: {text}");
        let loaded = read(&text);
        assert!(!loaded.autostart, "placeholder until the registry fills it");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn file_matches_the_documented_layout() {
        let (dir, path) = temp_path("layout");
        write(&path, &Settings::default()).expect("write");
        let text = fs::read_to_string(&path).expect("read back");
        for expected in [
            "[general]",
            "[power]",
            "[pause]",
            "restore_wallpaper_on_exit = true",
            "tray = true",
            "engine_mode = \"accurate\"",
            "profile = \"off\"",
            "on_battery = \"off\"",
            "fullscreen = true",
            "maximized = false",
        ] {
            assert!(text.contains(expected), "missing {expected} in:\n{text}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_file_yields_defaults_and_partial_file_gets_the_preset() {
        let (dir, _path) = temp_path("corrupt");
        // A partial file fills the rest from the documented defaults, and a
        // profile without an on_battery key gets the D9 preset on load.
        let partial = read("[power]\nprofile = \"battery\"\n");
        assert_eq!(partial.power_profile, PowerProfile::Battery);
        assert_eq!(
            partial.on_battery,
            BatteryAction::Pause,
            "D9 preset on load"
        );
        assert_eq!(partial.pause, PauseSettings::default());
        assert!(partial.tray);

        // An explicit on_battery key is respected, never refilled.
        let explicit = read("[power]\nprofile = \"battery\"\non_battery = \"off\"\n");
        assert_eq!(explicit.on_battery, BatteryAction::Off, "explicit wins");

        // Corrupt content is ignored, not fatal.
        let corrupt = read("not a toml file at all {{{");
        assert_eq!(corrupt, Settings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        // A hand-edited file with one table and one key still loads.
        let loaded = read("[pause]\nmaximized = true\n");
        assert!(loaded.pause.maximized);
        assert!(loaded.pause.fullscreen, "missing key uses the default");
        assert_eq!(loaded.power_profile, PowerProfile::Off);
        assert!(loaded.restore_wallpaper_on_exit);
        // A file written before the toggle existed keeps honest colour.
        assert_eq!(loaded.engine_mode, PresentationMode::Accurate);

        // An explicit engine_mode is read back (design-engine-mode §5).
        let efficient = read("[general]\nengine_mode = \"efficient\"\n");
        assert_eq!(efficient.engine_mode, PresentationMode::Efficient);
    }
}
