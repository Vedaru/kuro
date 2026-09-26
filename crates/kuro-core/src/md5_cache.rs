//! Persistent MD5 cache: a file that hasn't changed is never hashed twice.
//!
//! Entries are keyed by the manifest `dest` and validated on (size, mtime) —
//! the same staleness contract `make` and `git` use. A hit is trusted without
//! reading the file, so content rewritten in place with size and mtime
//! untouched is invisible to the cache: that is the standard trade-off for
//! skipping the read. The cache lives in the game folder's `.kuro_cache/` and
//! can be deleted at any time — it only ever saves I/O.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use kuro_api::Result;

use crate::state;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    size: u64,
    mtime_ns: u64,
    md5: String,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    /// Keys consulted or recorded since [`Md5Cache::load`]. Only these are
    /// persisted, so entries for paths the manifest has dropped age out.
    touched: HashSet<String>,
}

/// Cheaply clonable handle shared by the rayon verify pool and the repair
/// tasks; persists once via [`Md5Cache::save`].
#[derive(Clone)]
pub struct Md5Cache {
    path: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl Md5Cache {
    /// Load the cache for a game folder. A missing or corrupt cache file is an
    /// empty cache — at worst that costs the re-hash it was meant to save.
    pub fn load(game_folder: &Path) -> Md5Cache {
        let path = state::cache_dir(game_folder).join(state::MD5_CACHE_FILE);
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Md5Cache {
            path,
            inner: Arc::new(Mutex::new(Inner {
                entries,
                touched: HashSet::new(),
            })),
        }
    }

    /// True when the file at `p` hashes to `expected`. When (size, mtime)
    /// match a recorded hash the file is not read at all; otherwise it is
    /// hashed and recorded for next run.
    pub fn matches(&self, key: &str, p: &Path, meta: &std::fs::Metadata, expected: &str) -> bool {
        let Some(mtime_ns) = mtime_ns(meta) else {
            // No usable timestamp — hash, but don't record.
            return kuro_patch::md5_file(p).map(|h| h == expected).unwrap_or(false);
        };
        if let Some(md5) = self.lookup(key, meta.len(), mtime_ns) {
            return md5 == expected;
        }
        let Ok(md5) = kuro_patch::md5_file(p) else {
            return false;
        };
        self.store(key, meta.len(), mtime_ns, &md5);
        md5 == expected
    }

    /// Record a known-good hash for the file at `p` (e.g. right after a
    /// verified download), so the next verify does not re-read it.
    pub fn record_file(&self, key: &str, p: &Path, md5: &str) {
        if md5.is_empty() {
            return;
        }
        if let Ok(meta) = std::fs::metadata(p) {
            if let Some(mtime_ns) = mtime_ns(&meta) {
                self.store(key, meta.len(), mtime_ns, md5);
            }
        }
    }

    /// Persist the cache with a crash-safe swap: an interrupted save leaves
    /// the previous cache file intact.
    pub fn save(&self) -> Result<()> {
        let data = {
            let mut inner = self.inner.lock().expect("md5 cache poisoned");
            let touched = std::mem::take(&mut inner.touched);
            inner.entries.retain(|k, _| touched.contains(k));
            serde_json::to_string_pretty(&inner.entries)?
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, data)?;
        crate::atomic::safe_replace(&tmp, &self.path)
    }

    fn lookup(&self, key: &str, size: u64, mtime_ns: u64) -> Option<String> {
        let mut inner = self.inner.lock().expect("md5 cache poisoned");
        let hit = match inner.entries.get(key) {
            Some(e) if e.size == size && e.mtime_ns == mtime_ns => Some(e.md5.clone()),
            _ => None,
        };
        if hit.is_some() {
            inner.touched.insert(key.to_string());
        }
        hit
    }

    fn store(&self, key: &str, size: u64, mtime_ns: u64, md5: &str) {
        let mut inner = self.inner.lock().expect("md5 cache poisoned");
        inner.touched.insert(key.to_string());
        inner.entries.insert(
            key.to_string(),
            Entry {
                size,
                mtime_ns,
                md5: md5.to_string(),
            },
        );
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> Option<u64> {
    let d = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_nanos() as u64)
}
