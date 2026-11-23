//! File-backed operations that need no engine.
//!
//! Two callers, one implementation:
//!
//! * the **CLI direct mode** (Pw1) — `settings`/`library` subcommands when
//!   no Cascade process of ours is running (the routing guard lives in
//!   `main.rs`; this module never decides *when* it may run);
//! * the **battery listener** — same operations served over the pipe
//!   while the engine is off.
//!
//! Rules: never touch user media (`remove` edits the list
//! only), never start or stop a process, and never watch the files
//! Settings saves are atomic (`config.rs`); the daemon
//! remains the single writer whenever its pipe answers.

use std::path::{Path, PathBuf};

use cascade_core::assess::assess;
use cascade_core::ipc::{
    ItemAssessment, LibraryAddResult, LibraryItemsData, RejectedPath, Settings, SettingsPatch,
};
use cascade_core::library::{display_path, AddOutcome, Library, LibraryItem, VideoMetadata};
use cascade_core::Fit;

use crate::config;
use crate::daemon::ProbeFn;
use crate::device;
use crate::library::{file_proof, LibraryStore};
use crate::registry;

// settings ----------------------------------------------------------------

/// `settings.get` with no daemon: the file the daemon reads, with the same
/// Run-key fill the daemon performs at its wire edge.
pub fn settings_get() -> Settings {
    wire(config::load())
}

/// `settings.set` with no daemon: apply the patch
/// the CLI already validated (`build_patch` — the daemon-path validation)
/// and persist it. There is no live engine, so nothing applies in-process;
/// the value takes effect at the next start.
pub fn settings_apply(patch: SettingsPatch) -> Result<Settings, String> {
    let path =
        config::path().ok_or_else(|| "no writable application data directory".to_string())?;
    Ok(wire(apply_at(&path, patch)?))
}

/// The file half of [`settings_apply`], path-injected for tests. A failed
/// save is an error here: the CLI must never print `key = value` for a
/// change that did not stick (the daemon keeps its apply-anyway behaviour,
/// different caller, different honesty rule).
fn apply_at(path: &Path, patch: SettingsPatch) -> Result<Settings, String> {
    let mut settings = config::load_at(path);
    settings.apply_patch(patch);
    config::save_at(path, &settings)?;
    Ok(settings)
}

/// The wire view of a `Settings` value: `autostart` is filled from the Run
/// key, never from the file.
fn wire(mut settings: Settings) -> Settings {
    settings.autostart = registry::autostart_state(&exe_path()).registered;
    settings
}

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::new())
}

// library -----------------------------------------------------------------

/// `library.list` with no daemon: the stored list, in list order.
pub fn library_list() -> LibraryItemsData {
    LibraryItemsData {
        items: LibraryStore::open().load().items,
    }
}

/// `library.add` with no daemon: load → shared core → save. A save failure
/// is returned to the caller (the CLI prints it); the daemon path keeps its
/// best-effort persist (`persist_library` in `daemon.rs`).
pub fn library_add(paths: Vec<String>, probe: &ProbeFn) -> Result<LibraryAddResult, String> {
    library_add_with(&LibraryStore::open(), paths, probe)
}

/// `library.remove` with no daemon — edits the list only; the file on disk
/// is never touched. `None` = no such id (the caller renders
/// the same "run `cascade library ls`" wording as the daemon path).
pub fn library_remove(item_id: &str) -> Result<Option<LibraryItem>, String> {
    library_remove_with(&LibraryStore::open(), item_id)
}

/// `library.prune` with no daemon — the daemon's exact drop rule (an entry
/// leaves the list only when its file no longer exists).
pub fn library_prune() -> Result<u32, String> {
    library_prune_with(&LibraryStore::open())
}

fn library_add_with(
    store: &LibraryStore,
    paths: Vec<String>,
    probe: &ProbeFn,
) -> Result<LibraryAddResult, String> {
    let mut library = store.load();
    let result = library_add_into(&mut library, probe, paths);
    store.save(&library)?;
    Ok(result)
}

fn library_remove_with(store: &LibraryStore, item_id: &str) -> Result<Option<LibraryItem>, String> {
    let mut library = store.load();
    let removed = library.remove(item_id);
    if removed.is_some() {
        store.save(&library)?;
    }
    Ok(removed)
}

fn library_prune_with(store: &LibraryStore) -> Result<u32, String> {
    let mut library = store.load();
    let removed = library.prune(|path| Path::new(path).is_file());
    if removed > 0 {
        store.save(&library)?;
    }
    Ok(removed as u32)
}

/// The `library.add` core shared by the daemon and the offline path
/// (one implementation so the two can never drift):
/// canonicalise each path, check it is a file, probe its metadata (
/// probe-once through the stored `mtime`+`size` proof), dedupe by canonical
/// path, and compute the badge assessment for every accepted item.
/// Rejections are per-path — one bad file never fails the batch, and
/// nothing is ever copied or modified.
///
/// Persistence stays with the caller: the daemon saves best-effort, the
/// offline wrapper returns the save error to the user.
pub fn library_add_into(
    library: &mut Library,
    probe: &ProbeFn,
    paths: Vec<String>,
) -> LibraryAddResult {
    let mut result = LibraryAddResult {
        added: Vec::new(),
        rejected: Vec::new(),
        assessments: Vec::new(),
    };
    let seed = clock_seed();
    for (index, raw) in paths.into_iter().enumerate() {
        // Canonicalise for storage and dedupe; a failure here almost
        // always means the file does not exist (or is unreachable).
        let canonical = match std::fs::canonicalize(&raw) {
            Ok(path) => path,
            Err(err) => {
                result.rejected.push(RejectedPath {
                    path: raw,
                    reason: format!("file not found: {err}"),
                });
                continue;
            }
        };
        if !canonical.is_file() {
            result.rejected.push(RejectedPath {
                path: raw,
                reason: "not a file".to_string(),
            });
            continue;
        }
        let display = display_path(&canonical.to_string_lossy());
        let (mtime, size) = file_proof(&canonical);
        let cached = library
            .find_by_path(&display)
            .filter(|entry| entry.proof_matches(mtime, size))
            .map(LibraryItem::metadata);
        let meta = match cached {
            Some(meta) => meta,
            None => match probe(Path::new(&canonical)) {
                Ok(meta) => meta,
                Err(reason) => {
                    result.rejected.push(RejectedPath {
                        path: display,
                        reason,
                    });
                    continue;
                }
            },
        };
        let id = library.generate_id(seed.wrapping_add(index as u64));
        let mut item = LibraryItem::new(id, display, meta, clock_seed());
        if let (Some(mtime), Some(size)) = (mtime, size) {
            item = item.with_proof(mtime, size);
        }
        // `Added` and `Updated` (a re-add refreshes metadata) both land in
        // `added` — the existing entry is what the caller gets back.
        let item = match library.add(item) {
            AddOutcome::Added(item) | AddOutcome::Updated(item) => item,
        };
        result.added.push(item);
    }
    // One device collection per batch, only when something got in: the
    // primary monitor is the badge's default target (the GUI passes the
    // assigned one later).
    if !result.added.is_empty() {
        let device = device::collect();
        if let Ok(target) = device::pick_monitor(&device, None) {
            let target = target.clone();
            let entries: Vec<(String, String, VideoMetadata)> = result
                .added
                .iter()
                .map(|item| (item.id.clone(), item.path.clone(), item.metadata()))
                .collect();
            for (id, path, meta) in entries {
                let assessment = assess(&meta, &device, &target, Fit::Contain);
                result.assessments.push(ItemAssessment {
                    id,
                    path,
                    warnings: assessment.warnings,
                    recommendation: assessment.recommendation,
                });
            }
        }
    }
    result
}

/// Seed for id generation and `added_at`: wall-clock nanoseconds folded
/// with the process id so two daemons (in different sessions) do not march
/// in lockstep (moved from `daemon.rs` with the shared add core).
fn clock_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id()) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cascade-offline-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn meta() -> VideoMetadata {
        VideoMetadata {
            width: 1920,
            height: 1080,
            fps: Some(60.0),
            duration_ms: Some(3_000),
            bitrate_bps: None,
            codec: "H264".to_string(),
            profile: None,
            level: None,
            hdr: None,
        }
    }

    fn ok_probe() -> ProbeFn {
        Box::new(|_path: &Path| Ok(meta()))
    }

    // settings ------------------------------------------------------------

    #[test]
    fn settings_apply_persists_the_patch_and_returns_the_result() {
        let dir = temp_dir("settings");
        let path = dir.join("config.toml");
        let patch = SettingsPatch {
            volume: Some(42),
            ..SettingsPatch::default()
        };
        let settings = apply_at(&path, patch).expect("apply");
        assert_eq!(settings.volume, 42, "the patch is applied");

        // A second apply loads the first one back (read-modify-write).
        let patch = SettingsPatch {
            tray: Some(false),
            ..SettingsPatch::default()
        };
        let settings = apply_at(&path, patch).expect("second apply");
        assert_eq!(settings.volume, 42, "earlier keys survive");
        assert!(!settings.tray, "new key applied");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_apply_keeps_defaults_for_untouched_keys() {
        let dir = temp_dir("settings-defaults");
        let path = dir.join("config.toml");
        let patch = SettingsPatch {
            on_battery: Some(cascade_core::rule_engine::BatteryAction::Stop),
            ..SettingsPatch::default()
        };
        let settings = apply_at(&path, patch).expect("apply");
        assert_eq!(
            settings.on_battery,
            cascade_core::rule_engine::BatteryAction::Stop
        );
        assert!(
            settings.restore_wallpaper_on_exit,
            "documented default survives an unrelated patch"
        );
        assert_eq!(
            settings.engine_mode,
            cascade_core::PresentationMode::Accurate
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // library: add --------------------------------------------------------

    #[test]
    fn add_rejects_a_missing_file_without_failing_the_batch() {
        let dir = temp_dir("add-missing");
        let missing = dir.join("nope.mp4").display().to_string();
        let result = library_add_into(&mut Library::default(), &ok_probe(), vec![missing.clone()]);
        assert!(result.added.is_empty(), "nothing added");
        assert_eq!(result.rejected.len(), 1, "one rejection");
        assert_eq!(result.rejected[0].path, missing);
        assert!(result.rejected[0].reason.starts_with("file not found"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn add_rejects_a_directory() {
        let dir = temp_dir("add-dir");
        let sub = dir.join("subdir");
        std::fs::create_dir_all(&sub).expect("subdir");
        let result = library_add_into(
            &mut Library::default(),
            &ok_probe(),
            vec![sub.display().to_string()],
        );
        assert!(result.added.is_empty());
        assert_eq!(result.rejected[0].reason, "not a file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn add_probes_once_and_reuses_the_proof_on_readd() {
        let dir = temp_dir("add-proof");
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"not really a video").expect("write");
        let raw = file.display().to_string();

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counting: ProbeFn = {
            let calls = std::sync::Arc::clone(&calls);
            Box::new(move |_: &Path| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(meta())
            })
        };

        let mut library = Library::default();
        let first = library_add_into(&mut library, &counting, vec![raw.clone()]);
        assert_eq!(first.added.len(), 1, "accepted");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one probe");
        let item = &first.added[0];
        assert_eq!(item.width, 1920, "metadata from the probe");
        assert!(item.path.ends_with("clip.mp4"), "{}", item.path);

        // Same file, unchanged proof: no second Media Foundation touch.
        let second = library_add_into(&mut library, &counting, vec![raw]);
        assert_eq!(second.added.len(), 1, "re-add updates in place");
        assert_eq!(library.items.len(), 1, "deduped by canonical path");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "proof reused, no probe");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn add_rejects_when_the_probe_fails_and_keeps_the_rest() {
        let dir = temp_dir("add-probe-fail");
        let good = dir.join("good.mp4");
        std::fs::write(&good, b"x").expect("write");
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, b"x").expect("write");
        let failing: ProbeFn = Box::new(|path: &Path| {
            if path.ends_with("bad.mp4") {
                Err("codec not supported".to_string())
            } else {
                Ok(meta())
            }
        });
        let mut library = Library::default();
        let result = library_add_into(
            &mut library,
            &failing,
            vec![bad.display().to_string(), good.display().to_string()],
        );
        assert_eq!(result.added.len(), 1, "one good file got in");
        assert_eq!(result.rejected.len(), 1, "one rejection");
        assert_eq!(result.rejected[0].reason, "codec not supported");
        let _ = std::fs::remove_dir_all(dir);
    }

    // library: remove / prune --------------------------------------------

    #[test]
    fn remove_edits_the_list_and_the_file_follows() {
        let dir = temp_dir("remove");
        let store = LibraryStore::open_in(&dir);
        let mut library = Library::default();
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"x").expect("write");
        library_add_into(&mut library, &ok_probe(), vec![file.display().to_string()]);
        store.save(&library).expect("save");

        let removed = library_remove_with(&store, &library.items[0].id)
            .expect("remove")
            .expect("found");
        assert_eq!(removed.path, library.items[0].path);
        assert!(store.load().items.is_empty(), "list updated on disk");
        assert!(file.exists(), "the media file itself is untouched");

        let missing = library_remove_with(&store, "zzzz").expect("second remove");
        assert!(missing.is_none(), "unknown id is None, not an error");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_drops_only_entries_whose_files_are_gone() {
        let dir = temp_dir("prune");
        let store = LibraryStore::open_in(&dir);
        // The "file disappeared after it was added" case: one entry whose
        // file exists on disk, one whose file does not.
        let alive = dir.join("alive.mp4");
        std::fs::write(&alive, b"x").expect("write");
        let gone = dir.join("gone.mp4");
        let mut seeded = Library::default();
        seeded.add(LibraryItem::new(
            "a1b2".to_string(),
            gone.display().to_string(),
            meta(),
            1,
        ));
        seeded.add(LibraryItem::new(
            "c3d4".to_string(),
            alive.display().to_string(),
            meta(),
            2,
        ));
        store.save(&seeded).expect("save");

        let removed = library_prune_with(&store).expect("prune");
        assert_eq!(removed, 1, "only the missing file leaves the list");
        let after = store.load();
        assert_eq!(after.items.len(), 1);
        assert!(after.items[0].path.ends_with("alive.mp4"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_with_everything_present_is_a_noop() {
        let dir = temp_dir("prune-noop");
        let store = LibraryStore::open_in(&dir);
        let mut library = Library::default();
        let alive = dir.join("alive.mp4");
        std::fs::write(&alive, b"x").expect("write");
        library_add_into(&mut library, &ok_probe(), vec![alive.display().to_string()]);
        store.save(&library).expect("save");
        assert_eq!(library_prune_with(&store).expect("prune"), 0);
        assert_eq!(store.load().items.len(), 1, "nothing removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    // library: list -------------------------------------------------------

    #[test]
    fn list_reads_the_stored_order() {
        let dir = temp_dir("list");
        let store = LibraryStore::open_in(&dir);
        let mut library = Library::default();
        let first = LibraryItem::new("aa11".to_string(), r"C:\a.mp4".to_string(), meta(), 1);
        let second = LibraryItem::new("bb22".to_string(), r"C:\b.mp4".to_string(), meta(), 2);
        library.add(first);
        library.add(second);
        store.save(&library).expect("save");

        // `library_list()` uses the real store; assert the same read path
        // through the explicit store to stay off the real profile here.
        let items = store.load();
        assert_eq!(items.items.len(), 2);
        assert_eq!(items.items[0].id, "aa11");
        assert_eq!(items.items[1].id, "bb22");
        let _ = std::fs::remove_dir_all(dir);
    }
}
