//! Thumbnail worker for the library grid (design-m5-p4 §11 T5–T11).
//!
//! One dedicated thread in the **UI process** owns the disk cache and
//! the decode pipeline; the UI thread owns an LRU of GPU textures and is
//! the only place `load_texture` runs (T7). The daemon never links this
//! path (isolation DoD, design-m5 §11) — decode happens here, in the
//! window's process, and a failure is a placeholder tile, never a
//! banner (§11.1).
//!
//! The thread never calls into egui's API: it holds a clone of
//! `egui::Context` only to `request_repaint()` when a result is ready —
//! the S1-proven cross-thread wake.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::UNIX_EPOCH;

use cascade_media::thumbnail::{self, Frame, JPEG_QUALITY, THUMB_HEIGHT, THUMB_WIDTH};
use eframe::egui;

use crate::theme;

/// In-memory texture capacity (T10): ≈60 tiles ≈ two screens at the
/// default window size; past it the least-recently-rendered texture is
/// dropped (its `TextureHandle` frees the egui texture).
pub const LRU_LIMIT: usize = 60;

/// One decode request from the render thread (T6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbReq {
    /// Stable 4-hex library id — the cache key (`{id}.jpg`).
    pub id: String,
    /// Source file path, exactly as the library lists it.
    pub path: String,
}

/// One result back to the UI thread (T6/T7): RGBA bytes ready for
/// `ColorImage`, or the reason to show on hover over the placeholder.
#[derive(Debug, PartialEq, Eq)]
pub enum ThumbDone {
    Ready {
        id: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Failed {
        id: String,
        reason: String,
    },
}

/// The dedicated thumb thread (T6): serial, so one id is never decoded
/// twice at once; wakes the UI after every result (T6/T7).
pub fn thumb_worker(reqs: Receiver<ThumbReq>, done: Sender<ThumbDone>, ctx: egui::Context) {
    while let Ok(req) = reqs.recv() {
        let msg = match load_thumb(&req) {
            Ok((width, height, rgba)) => ThumbDone::Ready {
                id: req.id,
                width,
                height,
                rgba,
            },
            Err(reason) => ThumbDone::Failed { id: req.id, reason },
        };
        let _ = done.send(msg);
        // A finished thumbnail must appear now, not at the next 1 s
        // poll repaint (`design-m5-p4` §5 wake semantics).
        ctx.request_repaint();
    }
    // Request channel closed → the app flips `thumbs_live` and every
    // tile stays on its placeholder (§11.1: no retry loops).
}

/// One thumb: fresh stat first (T5), cache hit if the proof matches,
/// otherwise decode → fit → JPEG → cache write (§11.4 pipeline).
/// Returns the fitted RGBA frame either way — the texture comes from the
/// same bytes on both paths (T7).
fn load_thumb(req: &ThumbReq) -> Result<(u32, u32, Vec<u8>), String> {
    if !safe_id(&req.id) {
        // Never build cache paths from a malformed id (AGENTS §3.12).
        return Err("unusable library id".to_string());
    }
    let source = PathBuf::from(&req.path);
    // The proof must be the file state *now*, not the wire item's
    // possibly stale probe (T5).
    let stat = std::fs::metadata(&source).map_err(|err| format!("cannot read file: {err}"))?;
    let mtime = stat
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let size = stat.len();

    if let Some(dir) = thumbs_dir() {
        if let Some(frame) = cache_hit(&dir, &req.id, mtime, size) {
            return Ok((frame.width, frame.height, frame.rgba));
        }
    }

    // Miss (or stale): decode the first frame at native size, contain-
    // fit it to the cache size, encode the JPEG for next time.
    let frame = thumbnail::decode_first_frame(&source)?;
    // Letterbox pad = the tile's surface colour, so the pad blends into
    // the placeholder frame (Color32 has no `to_array`; this returns the
    // sRGB bytes, alpha included).
    let pad = theme::SURFACE.to_srgba_unmultiplied();
    let fitted = thumbnail::fit_contain(&frame, THUMB_WIDTH, THUMB_HEIGHT, pad)?;
    let jpeg = thumbnail::encode_jpeg(&fitted, JPEG_QUALITY)?;
    if let Some(dir) = thumbs_dir() {
        // Best effort: a full disk or read-only profile must not hide
        // the thumbnail that was just decoded — only skip the reuse.
        if let Err(err) = write_cache(&dir, &req.id, &jpeg, mtime, size) {
            eprintln!(
                "cascade: ui: thumbnail cache write failed for {}: {err}",
                req.id
            );
        }
    }
    Ok((fitted.width, fitted.height, fitted.rgba))
}

/// Cache hit (T5): `{id}.jpg` + `{id}.meta` present and the fresh stat
/// matches the proof. Every failure mode is a miss — regenerate, never a
/// wrong hit (T4).
fn cache_hit(dir: &Path, id: &str, mtime: u64, size: u64) -> Option<Frame> {
    let proof = std::fs::read_to_string(meta_path(dir, id)).ok()?;
    if parse_meta(&proof)? != (mtime, size) {
        return None;
    }
    let jpeg = std::fs::read(jpg_path(dir, id)).ok()?;
    thumbnail::decode_jpeg(&jpeg).ok()
}

/// The one-line sidecar proof `"<src_mtime> <src_size>"` (T4). Anything
/// else — missing fields, non-numbers, trailing junk — is a miss.
fn parse_meta(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.split_whitespace();
    let mtime = parts.next()?.parse().ok()?;
    let size = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((mtime, size))
}

/// Write `{id}.jpg` then `{id}.meta`, each tmp+rename. Order matters: a
/// crash between them leaves no/old meta → regenerate, never a wrong hit
/// (T4).
fn write_cache(dir: &Path, id: &str, jpeg: &[u8], mtime: u64, size: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(&jpg_path(dir, id), jpeg)?;
    let proof = format!("{mtime} {size}\n");
    write_atomic(&meta_path(dir, id), proof.as_bytes())
}

/// Write to `{name}.tmp` then rename over the target, so a crash never
/// leaves a half-written cache file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Delete cache files whose id left the library (T11): our own files in
/// our own directory only (AGENTS §3.12). Returns the number removed.
///
/// Called once, after the first *successful* `library.list` reply —
/// never before, and never from an error path (an empty guess would
/// wipe a valid cache).
///
/// Compiled only outside tests (`cfg(not(test))`, mirrored at the call
/// site in `app.rs`): a unit test must never touch the real cache
/// directory (AGENTS §7). The filter logic below is unit-tested; the
/// full prune is V4 on the target machine.
#[cfg(not(test))]
pub fn prune_orphans(current_ids: &[String]) -> usize {
    let Some(dir) = thumbs_dir() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let current: HashSet<&str> = current_ids.iter().map(String::as_str).collect();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = cache_file_id(name) else {
            continue;
        };
        if is_orphan(id, &current) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The id a cache filename belongs to, or `None` for anything that is
/// not one of our `{id}.jpg` / `{id}.meta` (or their `.tmp`) names with
/// a well-formed id — prune never deletes what we did not write.
fn cache_file_id(name: &str) -> Option<&str> {
    let id = name
        .strip_suffix(".jpg.tmp")
        .or_else(|| name.strip_suffix(".meta.tmp"))
        .or_else(|| name.strip_suffix(".jpg"))
        .or_else(|| name.strip_suffix(".meta"))?;
    safe_id(id).then_some(id)
}

/// A well-formed cache id is orphaned exactly when the library no
/// longer lists it (T11).
fn is_orphan(id: &str, current: &HashSet<&str>) -> bool {
    !current.contains(id)
}

/// Cache filenames are `{id}.jpg`/`{id}.meta` with the daemon's short
/// alphanumeric id — refuse anything else so a malformed id can never
/// point outside the thumbs directory (`..`, separators, dots).
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 8 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `%LOCALAPPDATA%\Cascade\thumbs`, or `None` when the environment is
/// unusable — thumbnails then still decode in memory, they just are not
/// reused across restarts.
fn thumbs_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| PathBuf::from(home).join("AppData").join("Local"))
        })?;
    Some(base.join("Cascade").join("thumbs"))
}

fn jpg_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jpg"))
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.meta"))
}

/// In-memory texture LRU (T10): `HashMap` for lookup, `VecDeque` for
/// recency — `get_touch` on every render, evict from the front past
/// `limit` (visible tiles were just touched, so the front can never be
/// one). Dropping the handle frees the egui texture.
///
/// Generic over the handle so the ordering logic unit-tests without a
/// renderer (§11.5 V7); the app instantiates it with
/// `egui::TextureHandle`.
pub struct TextureLru<H> {
    textures: HashMap<String, H>,
    order: VecDeque<String>,
    limit: usize,
}

impl<H> TextureLru<H> {
    pub fn new(limit: usize) -> Self {
        Self {
            textures: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    /// The handle for `id`, marked most-recently-used (T10).
    pub fn get_touch(&mut self, id: &str) -> Option<&H> {
        self.touch(id);
        self.textures.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.textures.contains_key(id)
    }

    /// Insert (or replace) the handle for `id` as most-recently-used and
    /// evict from the front while over `limit`.
    pub fn insert(&mut self, id: String, handle: H) {
        self.remove_id(&id);
        self.textures.insert(id.clone(), handle);
        self.order.push_back(id);
        while self.order.len() > self.limit {
            if let Some(oldest) = self.order.pop_front() {
                self.textures.remove(&oldest);
            }
        }
    }

    /// Drop everything whose id left the library (T9): no texture lingers
    /// for an item that is gone.
    pub fn retain_ids(&mut self, current: &HashSet<String>) {
        self.order.retain(|id| current.contains(id));
        self.textures.retain(|id, _| current.contains(id));
    }

    fn remove_id(&mut self, id: &str) {
        if self.textures.remove(id).is_some() {
            self.order.retain(|entry| entry != id);
        }
    }

    fn touch(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|entry| entry == id) {
            if pos + 1 < self.order.len() {
                if let Some(entry) = self.order.remove(pos) {
                    self.order.push_back(entry);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    // --- meta proof (T4) ------------------------------------------------

    #[test]
    fn meta_parses_the_documented_one_line_proof() {
        assert_eq!(parse_meta("1700000000 12345\n"), Some((1700000000, 12345)));
        assert_eq!(parse_meta("1700000000 12345"), Some((1700000000, 12345)));
    }

    #[test]
    fn meta_rejects_missing_or_junk_proof() {
        assert_eq!(parse_meta(""), None);
        assert_eq!(parse_meta("1700000000"), None);
        assert_eq!(parse_meta("abc def"), None);
        assert_eq!(parse_meta("1 2 3"), None);
    }

    // --- prune filter (T11) --------------------------------------------

    #[test]
    fn cache_names_only_claim_our_files() {
        assert_eq!(cache_file_id("ab12.jpg"), Some("ab12"));
        assert_eq!(cache_file_id("ab12.meta"), Some("ab12"));
        assert_eq!(cache_file_id("ab12.jpg.tmp"), Some("ab12"));
        assert_eq!(cache_file_id("ab12.meta.tmp"), Some("ab12"));
        assert_eq!(cache_file_id("notes.txt"), None);
        assert_eq!(cache_file_id("ab12.jpg.bak"), None);
        assert_eq!(cache_file_id("../evil.jpg"), None);
        assert_eq!(cache_file_id(".jpg"), None);
    }

    #[test]
    fn orphan_filter_is_set_membership_only() {
        let current: HashSet<&str> = ["ab12", "cd34"].into_iter().collect();
        assert!(!is_orphan("ab12", &current));
        assert!(is_orphan("ff00", &current));
    }

    #[test]
    fn safe_id_accepts_short_alphanumeric_ids_only() {
        assert!(safe_id("ab12"));
        assert!(safe_id("ABCD"));
        assert!(!safe_id(""));
        assert!(!safe_id("ab/cd"));
        assert!(!safe_id("abcd.png"));
        assert!(!safe_id("abcdefghi"));
    }

    // --- LRU order (T10) ------------------------------------------------

    #[test]
    fn lru_evicts_the_front_past_the_limit() {
        let mut lru: TextureLru<u32> = TextureLru::new(3);
        for id in ["a", "b", "c"] {
            lru.insert(id.to_string(), 0);
        }
        lru.insert("d".to_string(), 0);
        assert!(!lru.contains("a"), "front must be evicted first");
        assert!(lru.contains("b") && lru.contains("c") && lru.contains("d"));
        assert_eq!(lru.order, ids(&["b", "c", "d"]));
    }

    #[test]
    fn lru_touch_moves_a_visible_id_to_the_back() {
        let mut lru: TextureLru<u32> = TextureLru::new(3);
        for id in ["a", "b", "c"] {
            lru.insert(id.to_string(), 0);
        }
        assert!(lru.get_touch("a").is_some());
        lru.insert("d".to_string(), 0);
        assert!(lru.contains("a"), "touched id must survive eviction");
        assert!(!lru.contains("b"), "untouched front is evicted");
        assert_eq!(lru.order, ids(&["c", "a", "d"]));
    }

    #[test]
    fn lru_retain_drops_ids_that_left_the_library() {
        let mut lru: TextureLru<u32> = TextureLru::new(3);
        for id in ["a", "b", "c"] {
            lru.insert(id.to_string(), 0);
        }
        let current: HashSet<String> = ["b"].into_iter().map(str::to_string).collect();
        lru.retain_ids(&current);
        assert!(!lru.contains("a") && !lru.contains("c"));
        assert!(lru.contains("b"));
        assert_eq!(lru.order, ids(&["b"]));
    }

    #[test]
    fn lru_insert_replaces_without_duplicating_order() {
        let mut lru: TextureLru<u32> = TextureLru::new(2);
        lru.insert("a".to_string(), 1);
        lru.insert("a".to_string(), 2);
        assert_eq!(lru.order, ids(&["a"]));
        lru.insert("b".to_string(), 3);
        assert_eq!(lru.order, ids(&["a", "b"]));
        assert_eq!(lru.textures.get("a"), Some(&2));
    }

    #[test]
    fn lru_get_touch_on_a_missing_id_is_none() {
        let mut lru: TextureLru<u32> = TextureLru::new(2);
        assert_eq!(lru.get_touch("nope"), None);
    }
}
