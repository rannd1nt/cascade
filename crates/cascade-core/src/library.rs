//! Library model: a list of **paths only**.
//!
//! Nothing here reads, copies, moves, or modifies user media —
//! the model is pure data plus list algebra so every rule (dedupe, id
//! stability, prune) is unit-testable without Win32 or a file system. The
//! daemon owns persistence (`cascade-win/src/library.rs`) and probing
//! (`cascade-media::probe`).

use serde::{Deserialize, Serialize};

/// One library entry (extended with the assessment
/// fields from ). The file at `path` is referenced, never
/// copied; metadata is a snapshot taken at `added_at` time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryItem {
    /// 4 hex characters, unique in the file, stable for this entry
    /// re-adding the path keeps the id.
    pub id: String,
    /// Canonical path without the `\\?\` verbatim prefix.
    pub path: String,
    /// File stem, shown in the GUI grid and `library list`.
    pub title: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// Fourcc-style codec name (`H264`, `HEVC`, `AV1`, …).
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// HDR transfer function found in the container: `Some(true)` = PQ/HLG,
    /// `Some(false)` = a known SDR transfer, `None` = attribute absent
    /// assessment input, additive field — old files decode without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<bool>,
    /// Staleness proof: file modification time in Unix seconds at
    /// probe time, paired with `size`. Same path + different proof = the
    /// file was replaced (4K → 1080p) and is re-probed on the next touch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<u64>,
    /// Staleness proof: file size in bytes at probe time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Unix seconds when the entry was first added.
    pub added_at: u64,
}

/// Metadata a probe reads off one file (entry fields,
/// without the identity the daemon assigns). Produced by
/// `cascade_media::probe` and stored verbatim on [`LibraryItem`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoMetadata {
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// Fourcc-style codec name (`H264`, `HEVC`, `AV1`, …).
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// HDR transfer function found in the container (see [`LibraryItem::hdr`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<bool>,
}

impl LibraryItem {
    /// Assemble an entry from what the daemon knows: the id it generated,
    /// the canonical path, the probe's metadata, and the clock.
    pub fn new(id: String, path: String, meta: VideoMetadata, added_at: u64) -> Self {
        let title = title_for(&path);
        Self {
            id,
            path,
            title,
            width: meta.width,
            height: meta.height,
            fps: meta.fps,
            duration_ms: meta.duration_ms,
            bitrate_bps: meta.bitrate_bps,
            codec: meta.codec,
            profile: meta.profile,
            level: meta.level,
            hdr: meta.hdr,
            mtime: None,
            size: None,
            added_at,
        }
    }

    /// Attach the file's staleness proof: the `mtime` + `size` the
    /// probe saw, so the next touch can tell a replaced file at the same
    /// path from an unchanged one without opening it.
    #[must_use]
    pub fn with_proof(mut self, mtime: u64, size: u64) -> Self {
        self.mtime = Some(mtime);
        self.size = Some(size);
        self
    }

    /// Does the stored proof still describe the file on disk? An entry
    /// probed before (proof `None` on either side) is always stale —
    /// re-probe once rather than trust metadata of unknown vintage.
    pub fn proof_matches(&self, mtime: Option<u64>, size: Option<u64>) -> bool {
        match (self.mtime, self.size, mtime, size) {
            (Some(item_mtime), Some(item_size), Some(mtime), Some(size)) => {
                item_mtime == mtime && item_size == size
            }
            _ => false,
        }
    }

    /// The stored metadata back in probe shape — what `assess()` consumes
    /// when the staleness proof still matches : no re-probe needed).
    pub fn metadata(&self) -> VideoMetadata {
        VideoMetadata {
            width: self.width,
            height: self.height,
            fps: self.fps,
            duration_ms: self.duration_ms,
            bitrate_bps: self.bitrate_bps,
            codec: self.codec.clone(),
            profile: self.profile.clone(),
            level: self.level.clone(),
            hdr: self.hdr,
        }
    }
}

/// The library as a whole — the wire payload is `{"items":[...]}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub items: Vec<LibraryItem>,
}

/// What one `library.add` did with a path: a brand-new entry, or a
/// metadata refresh of the entry that already had this path.
#[derive(Clone, Debug, PartialEq)]
pub enum AddOutcome {
    Added(LibraryItem),
    Updated(LibraryItem),
}

impl Library {
    pub fn find(&self, id: &str) -> Option<&LibraryItem> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn find_by_path(&self, path: &str) -> Option<&LibraryItem> {
        let key = dedupe_key(path);
        self.items.iter().find(|item| dedupe_key(&item.path) == key)
    }

    /// Insert a new entry, or refresh the existing one with the same
    /// canonical path (dedupe): the id and `added_at` of the
    /// existing entry survive, the metadata is replaced.
    pub fn add(&mut self, item: LibraryItem) -> AddOutcome {
        let key = dedupe_key(&item.path);
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|entry| dedupe_key(&entry.path) == key)
        {
            let mut updated = item;
            updated.id = existing.id.clone();
            updated.added_at = existing.added_at;
            *existing = updated;
            return AddOutcome::Updated(existing.clone());
        }
        self.items.push(item.clone());
        AddOutcome::Added(item)
    }

    /// "Remove from library" — edits the list only.
    pub fn remove(&mut self, id: &str) -> Option<LibraryItem> {
        let index = self.items.iter().position(|item| item.id == id)?;
        Some(self.items.remove(index))
    }

    /// Drop every entry whose path no longer exists.
    /// Returns how many were removed. The `exists` closure keeps this
    /// pure — tests pass a set, the daemon passes `Path::is_file`.
    pub fn prune(&mut self, exists: impl Fn(&str) -> bool) -> usize {
        let before = self.items.len();
        self.items.retain(|item| exists(&item.path));
        before - self.items.len()
    }

    /// A fresh 4-hex id not used by any current entry. `seed` makes the
    /// sequence deterministic in tests; the daemon seeds it from the clock.
    pub fn generate_id(&self, seed: u64) -> String {
        let mut state = seed | 1; // never collapse to a fixed sequence
        loop {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let candidate = format!("{:04x}", ((state >> 32) as u32) & 0xFFFF);
            if self.find(&candidate).is_none() {
                return candidate;
            }
        }
    }
}

/// The dedupe/lookup key for a path: verbatim prefixes stripped, separators
/// unified, lower-cased (Windows paths are case-insensitive). Pure so the
/// dedupe rule is testable without touching the file system.
pub fn dedupe_key(path: &str) -> String {
    let stripped = path
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| path.strip_prefix(r"\\?\").map(str::to_string))
        .unwrap_or_else(|| path.to_string());
    stripped.replace('/', "\\").to_lowercase()
}

/// Strip the `\\?\` prefix from a canonical path for storage and display.
pub fn display_path(canonical: &str) -> String {
    canonical
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| canonical.strip_prefix(r"\\?\").map(str::to_string))
        .unwrap_or_else(|| canonical.to_string())
}

/// Title for a path: the file stem (`Furina Midnight Maedow.mp4` →
/// `Furina Midnight Maedow`). Falls back to the whole file name when the
/// stem is empty (a file named `.hidden`).
pub fn title_for(path: &str) -> String {
    let name = std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    std::path::Path::new(&name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, id: &str) -> LibraryItem {
        LibraryItem {
            id: id.to_string(),
            path: path.to_string(),
            title: title_for(path),
            width: 1920,
            height: 1080,
            fps: Some(29.97),
            duration_ms: Some(60_000),
            bitrate_bps: Some(8_000_000),
            codec: "H264".to_string(),
            profile: Some("100".to_string()),
            level: Some("41".to_string()),
            hdr: Some(false),
            mtime: None,
            size: None,
            added_at: 1_700_000_000,
        }
    }

    #[test]
    fn add_appends_and_find_returns_it() {
        let mut library = Library::default();
        let outcome = library.add(item(r"C:\videos\a.mp4", "a1b2"));
        assert!(matches!(outcome, AddOutcome::Added(_)));
        assert_eq!(library.items.len(), 1);
        assert_eq!(library.find("a1b2").unwrap().title, "a");
        assert!(library.find("ffff").is_none());
    }

    #[test]
    fn re_adding_the_same_path_updates_metadata_and_keeps_the_id() {
        let mut library = Library::default();
        library.add(item(r"C:\videos\a.mp4", "a1b2"));

        // Same file, different case and separator: still the same entry.
        let mut changed = item(r"c:/videos/A.MP4", "ffff");
        changed.width = 3840;
        changed.added_at = 1_800_000_000;
        let outcome = library.add(changed);

        assert!(matches!(outcome, AddOutcome::Updated(_)));
        assert_eq!(library.items.len(), 1, "dedupe by canonical path");
        let stored = library.find("a1b2").expect("the original id survives");
        assert_eq!(stored.width, 3840, "metadata refreshed");
        assert_eq!(stored.added_at, 1_700_000_000, "added_at is stable");
    }

    #[test]
    fn remove_edits_the_list_only() {
        let mut library = Library::default();
        library.add(item(r"C:\videos\a.mp4", "a1b2"));
        library.add(item(r"C:\videos\b.mp4", "c3d4"));

        let removed = library.remove("a1b2").expect("removed");
        assert_eq!(removed.path, r"C:\videos\a.mp4");
        assert_eq!(library.items.len(), 1);
        assert!(library.remove("a1b2").is_none(), "removing twice fails");
        assert!(library.remove("nope").is_none());
    }

    #[test]
    fn prune_drops_only_missing_paths_and_counts_them() {
        let mut library = Library::default();
        library.add(item(r"C:\videos\present.mp4", "aa11"));
        library.add(item(r"C:\videos\gone.mp4", "bb22"));
        library.add(item(r"C:\videos\also-gone.mp4", "cc33"));

        let present = [r"C:\videos\present.mp4"];
        let removed = library.prune(|path| present.contains(&path));
        assert_eq!(removed, 2);
        assert_eq!(library.items.len(), 1);
        assert!(library.find("aa11").is_some());
    }

    #[test]
    fn generate_id_is_four_hex_and_unique_within_the_file() {
        let mut library = Library::default();
        library.add(item(r"C:\videos\a.mp4", "0000"));
        library.add(item(r"C:\videos\b.mp4", "0001"));

        let first = library.generate_id(0xDEAD_BEEF);
        assert_eq!(first.len(), 4);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(library.find(&first).is_none(), "never collides");

        // Deterministic for a given seed, and a salted retry escapes a
        // seed whose first candidate is already taken.
        assert_eq!(library.generate_id(0xDEAD_BEEF), first);
        assert_ne!(library.generate_id(0xDEAD_BEEF + 1), "zzzz");
    }

    #[test]
    fn dedupe_key_strips_verbatim_prefixes_and_folds_case() {
        assert_eq!(
            dedupe_key(r"\\?\C:\Videos\Clip.MP4"),
            dedupe_key(r"c:\videos\clip.mp4")
        );
        assert_eq!(
            dedupe_key(r"\\?\UNC\server\share\a.mp4"),
            dedupe_key(r"\\server\share\A.MP4")
        );
        assert_ne!(dedupe_key(r"C:\a.mp4"), dedupe_key(r"C:\b.mp4"));
        assert_eq!(display_path(r"\\?\C:\Videos\a.mp4"), r"C:\Videos\a.mp4");
        assert_eq!(display_path(r"\\?\UNC\s\sh\a.mp4"), r"\\s\sh\a.mp4");
        assert_eq!(display_path(r"C:\a.mp4"), r"C:\a.mp4");
    }

    #[test]
    fn title_for_uses_the_file_stem() {
        assert_eq!(
            title_for(r"C:\Videos\Furina Midnight Maedow.mp4"),
            "Furina Midnight Maedow"
        );
        assert_eq!(title_for(r"C:\Videos\.hidden"), ".hidden");
        assert_eq!(title_for("no-extension"), "no-extension");
    }

    #[test]
    fn item_json_shape_is_the_documented_one() {
        // the `items` array carries exactly these keys; a
        // change here is a wire change.
        let text = serde_json::to_string(&item(r"C:\videos\a.mp4", "a1b2")).expect("serialize");
        for key in [
            r#""id""#,
            r#""path""#,
            r#""title""#,
            r#""width""#,
            r#""height""#,
            r#""fps""#,
            r#""duration_ms""#,
            r#""bitrate_bps""#,
            r#""codec""#,
            r#""profile""#,
            r#""level""#,
            r#""added_at""#,
        ] {
            assert!(text.contains(key), "missing {key} in {text}");
        }
        // The staleness proof is additive: unset fields are omitted from
        // the wire (a pre- library document has neither key).
        assert!(!text.contains("mtime"), "{text}");
        assert!(!text.contains(r#""size""#), "{text}");
        assert!(text.contains(r#""hdr""#), "{text}");
        // A file without optional attributes decodes (serde defaults).
        let minimal = r#"{"id":"aa11","path":"C:\\a.mp4","title":"a",
            "width":1,"height":1,"codec":"H264","added_at":0}"#;
        let decoded: LibraryItem =
            serde_json::from_str(minimal).expect("optional fields are optional");
        assert!(decoded.fps.is_none());
        assert!(decoded.duration_ms.is_none());
        // The whole library document round-trips.
        let library = Library {
            items: vec![decoded],
        };
        let back: Library =
            serde_json::from_str(&serde_json::to_string(&library).expect("json")).expect("decode");
        assert_eq!(back, library);
    }

    /// `mtime` + `size` decide whether a stored descriptor still
    /// describes the file on disk — the whole re-probe trigger, pure.
    #[test]
    fn staleness_proof_matches_only_when_both_numbers_are_the_same() {
        let proofed = item(r"C:\videos\a.mp4", "a1b2").with_proof(1_700_000_123, 42_424_242);
        // Identical proof → the cached metadata stands, no re-probe.
        assert!(proofed.proof_matches(Some(1_700_000_123), Some(42_424_242)));
        // Same path, replaced content (4K → 1080p): either number moved.
        assert!(!proofed.proof_matches(Some(1_700_000_999), Some(42_424_242)));
        assert!(!proofed.proof_matches(Some(1_700_000_123), Some(1)));
        // Unreadable proof on either side is "stale", never "fresh".
        assert!(!proofed.proof_matches(None, Some(42_424_242)));
        assert!(!proofed.proof_matches(Some(1_700_000_123), None));
        // A pre- entry (no stored proof) re-probes once, then is stable.
        let legacy = item(r"C:\videos\b.mp4", "c3d4");
        assert!(!legacy.proof_matches(Some(1), Some(1)));
    }

    /// The stored entry round-trips back into probe shape for `assess()`
    /// without touching the file : probe-once).
    #[test]
    fn metadata_rebuilds_the_probe_result_from_the_stored_entry() {
        let mut stored = item(r"C:\videos\a.mp4", "a1b2");
        stored.hdr = Some(true);
        let meta = stored.metadata();
        assert_eq!(meta.width, stored.width);
        assert_eq!(meta.height, stored.height);
        assert_eq!(meta.codec, stored.codec);
        assert_eq!(meta.profile.as_deref(), Some("100"));
        assert_eq!(meta.hdr, Some(true));
        // new() carries hdr over; the proof attaches afterwards.
        let fresh = LibraryItem::new(
            "ee77".to_string(),
            r"C:\videos\new.mp4".to_string(),
            meta,
            1_800_000_000,
        )
        .with_proof(7, 9);
        assert_eq!(fresh.hdr, Some(true));
        assert_eq!((fresh.mtime, fresh.size), (Some(7), Some(9)));
    }
}
