//! Persistent daemon state (`state.json`).
//!
//! The file has one job: tell the next start whether the previous session
//! ended cleanly, so a crash or a forced reboot can be repaired on the spot
//! instead of leaving the desktop in a stale state. It also records the
//! options the daemon was started with, which is what the future library and
//! IPC layers need to keep the UI and the daemon in agreement.
//!
//! It is deliberately small, human-readable, and written atomically
//! (`write tmp` + `rename`). A corrupt or unknown-version file is treated as
//! "no state" and never blocks startup.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Bumped whenever the meaning of a field changes.
pub const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub version: u32,
    /// `true` while a daemon is supposed to be running. A `true` value found
    /// on startup means the previous session died without a clean shutdown.
    pub running: bool,
    pub pid: u32,
    pub updated_unix: u64,
    /// `RRGGBB`, for the slice-1 solid background.
    pub color: String,
    pub repaint_level: u32,
    pub host_mode: String,
}

impl DaemonState {
    pub fn running_now(pid: u32, color: String, repaint_level: u32, host_mode: String) -> Self {
        Self {
            version: STATE_VERSION,
            running: true,
            pid,
            updated_unix: now_unix(),
            color,
            repaint_level,
            host_mode,
        }
    }

    pub fn stopped(&self) -> Self {
        let mut next = self.clone();
        next.running = false;
        next.updated_unix = now_unix();
        next
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `%LOCALAPPDATA%\Fontaine`, or `None` when the environment is unusable.
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| PathBuf::from(home).join("AppData").join("Local"))
        })?;
    Some(base.join("Fontaine"))
}

/// Handle to `state.json`. All methods are best-effort: a missing file or a
/// read-only directory must never stop the daemon.
pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    pub fn open() -> Self {
        Self {
            path: data_dir().map(|dir| dir.join("state.json")),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn load(&self) -> Option<DaemonState> {
        let path = self.path.as_ref()?;
        let text = fs::read_to_string(path).ok()?;
        match serde_json::from_str::<DaemonState>(&text) {
            Ok(state) if state.version == STATE_VERSION => Some(state),
            Ok(state) => {
                crate::derr!(
                    "state: ignoring {} (version {}, expected {STATE_VERSION})",
                    path.display(),
                    state.version
                );
                None
            }
            Err(err) => {
                crate::derr!("state: ignoring unreadable {}: {err}", path.display());
                None
            }
        }
    }

    pub fn save(&self, state: &DaemonState) -> Result<(), String> {
        let Some(path) = self.path.as_ref() else {
            return Err("no writable data directory".to_string());
        };
        let Some(dir) = path.parent() else {
            return Err("state path has no parent".to_string());
        };
        fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
        let text =
            serde_json::to_string_pretty(state).map_err(|err| format!("serialise state: {err}"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|err| format!("write {}: {err}", tmp.display()))?;
        // `fs::rename` on Windows is MoveFileEx(REPLACE_EXISTING): atomic.
        fs::rename(&tmp, path).map_err(|err| {
            let _ = fs::remove_file(&tmp);
            format!("replace {}: {err}", path.display())
        })
    }
}
