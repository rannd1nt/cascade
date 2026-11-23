//! Library persistence (`%LOCALAPPDATA%\Cascade\library.json`,
//!
//!
//! Follows the `state.rs` pattern exactly: best-effort, atomic (tmp +
//! rename), a corrupt or unknown-version file decodes to an **empty
//! library** and never blocks the daemon. The file holds the list only —
//! no user media is ever touched.
//!
//! Writers: the daemon main loop **or** the offline CLI /
//! battery listener when no full daemon exists — the routing guard in
//! makes those mutually exclusive (pipe wins).

use std::fs;
use std::path::{Path, PathBuf};

use cascade_core::library::Library;
use serde::{Deserialize, Serialize};

/// Bumped whenever the meaning of a field changes.
pub const LIBRARY_VERSION: u32 = 1;

/// On-disk wrapper: a version tag so a future format change can refuse an
/// old file instead of guessing (`state.rs` does the same).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct LibraryFile {
    version: u32,
    #[serde(default)]
    items: Vec<cascade_core::library::LibraryItem>,
}

/// Handle to `library.json`. All methods are best-effort: a missing file
/// or a read-only profile dir costs persistence, never the running daemon.
pub struct LibraryStore {
    path: Option<PathBuf>,
}

impl LibraryStore {
    pub fn open() -> Self {
        Self {
            path: crate::state::data_dir().map(|dir| dir.join("library.json")),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The stored list, or an empty library for a missing, corrupt, or
    /// unknown-version file : corrupt file = empty library).
    pub fn load(&self) -> Library {
        let Some(path) = self.path.as_ref() else {
            return Library::default();
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            // Missing file is the normal first-run case: no log spam.
            Err(_) if !path.exists() => return Library::default(),
            Err(err) => {
                crate::derr!("library: ignoring {}: {err}", path.display());
                return Library::default();
            }
        };
        match serde_json::from_str::<LibraryFile>(&text) {
            Ok(file) if file.version == LIBRARY_VERSION => Library { items: file.items },
            Ok(file) => {
                crate::derr!(
                    "library: ignoring {} (version {}, expected {LIBRARY_VERSION})",
                    path.display(),
                    file.version
                );
                Library::default()
            }
            Err(err) => {
                crate::derr!("library: ignoring unreadable {}: {err}", path.display());
                Library::default()
            }
        }
    }

    pub fn save(&self, library: &Library) -> Result<(), String> {
        let Some(path) = self.path.as_ref() else {
            return Err("no writable data directory".to_string());
        };
        let Some(dir) = path.parent() else {
            return Err("library path has no parent".to_string());
        };
        fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
        let file = LibraryFile {
            version: LIBRARY_VERSION,
            items: library.items.clone(),
        };
        let text = serde_json::to_string_pretty(&file)
            .map_err(|err| format!("serialise library: {err}"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|err| format!("write {}: {err}", tmp.display()))?;
        // `fs::rename` on Windows is MoveFileEx(REPLACE_EXISTING): atomic.
        fs::rename(&tmp, path).map_err(|err| {
            let _ = fs::remove_file(&tmp);
            format!("replace {}: {err}", path.display())
        })
    }

    /// Test seam: a store rooted at an explicit directory, so offline
    /// tests never touch the real profile.
    #[cfg(test)]
    pub(crate) fn open_in(dir: &Path) -> Self {
        Self {
            path: Some(dir.join("library.json")),
        }
    }
}

/// The staleness proof for one file: modification time in Unix
/// seconds and byte size. Either side can fail to read (unreadable
/// metadata, pre-epoch mtime) — `None` on that side means "cannot prove
/// freshness", and [`LibraryItem::proof_matches`] then treats the entry
/// as stale instead of guessing. Shared by the daemon's descriptor
/// refresh and the offline `library.add` (moved from `daemon.rs`,
///
pub fn file_proof(path: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs());
    (mtime, Some(meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_core::library::{LibraryItem, VideoMetadata};

    /// A store in a private temp directory, cleaned before use.
    fn temp_store(name: &str) -> (LibraryStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "cascade-library-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = LibraryStore {
            path: Some(dir.join("library.json")),
        };
        (store, dir)
    }

    fn item(id: &str, path: &str) -> LibraryItem {
        LibraryItem::new(
            id.to_string(),
            path.to_string(),
            VideoMetadata {
                width: 1920,
                height: 1080,
                fps: Some(30.0),
                duration_ms: Some(1_000),
                bitrate_bps: None,
                codec: "H264".to_string(),
                profile: None,
                level: None,
                hdr: None,
            },
            1_700_000_000,
        )
    }

    #[test]
    fn missing_file_is_an_empty_library() {
        let (store, dir) = temp_store("missing");
        let library = store.load();
        assert!(library.items.is_empty(), "first run has no library");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (store, dir) = temp_store("roundtrip");
        let mut library = Library::default();
        library.add(item("a1b2", r"C:\videos\clip.mp4"));
        store.save(&library).expect("save");

        let loaded = store.load();
        assert_eq!(loaded, library, "library survives a restart");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_file_yields_an_empty_library_and_never_blocks() {
        let (store, dir) = temp_store("corrupt");
        fs::create_dir_all(dir.join("..").join("nothing")).ok();
        let path = store.path().expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(path, "not json at all {{{").expect("write");
        assert!(store.load().items.is_empty(), "corrupt = empty library");

        // Unknown version: refuse instead of guessing.
        fs::write(path, r#"{"version":99,"items":[]}"#).expect("write");
        assert!(store.load().items.is_empty(), "version 99 refused");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_creates_the_missing_directory() {
        let (store, dir) = temp_store("mkdir");
        let mut library = Library::default();
        library.add(item("a1b2", r"C:\videos\clip.mp4"));
        store.save(&library).expect("save creates dirs");
        assert!(store.path().expect("path").is_file());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn file_holds_the_version_and_the_items() {
        let (store, dir) = temp_store("shape");
        let mut library = Library::default();
        library.add(item("a1b2", r"C:\videos\clip.mp4"));
        store.save(&library).expect("save");
        let text = fs::read_to_string(store.path().expect("path")).expect("read");
        assert!(text.contains(r#""version": 1"#), "{text}");
        assert!(text.contains(r#""id": "a1b2""#), "{text}");
        assert!(text.contains("clip.mp4"), "{text}");
        let _ = fs::remove_dir_all(dir);
    }
}
