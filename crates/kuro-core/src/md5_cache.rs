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
use std::sync::{Arc, Mutex, MutexGuard};
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

    /// Lock the cache, recovering from a poisoned mutex.
    ///
    /// The cache is disposable state whose only job is to save hashing, so a
    /// thread that panicked while holding the lock must not take the rest of
    /// the verify run down with it. Its contents are dropped instead of
    /// trusted: a panic mid-update can leave entries half-written, and a stale
    /// entry is only ever caught later by re-hashing a multi-GB pak. The poison
    /// flag is cleared too, or every later lock would throw away the entries
    /// recorded after the panic.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.entries.clear();
                guard.touched.clear();
                self.inner.clear_poison();
                guard
            }
        }
    }

    /// Persist the cache with a crash-safe swap: an interrupted save leaves
    /// the previous cache file intact.
    pub fn save(&self) -> Result<()> {
        let data = {
            let mut inner = self.lock();
            let touched = std::mem::take(&mut inner.touched);
            inner.entries.retain(|k, _| touched.contains(k));
            serde_json::to_string_pretty(&inner.entries)?
        };
        let tmp = crate::path::tmp_sibling(&self.path, "write");
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, data)?;
        crate::atomic::safe_replace(&tmp, &self.path)
    }

    fn lookup(&self, key: &str, size: u64, mtime_ns: u64) -> Option<String> {
        let mut inner = self.lock();
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
        let mut inner = self.lock();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic while another thread held the lock must not make every later
    /// lookup panic too: the cache is disposable, so it recovers empty.
    #[test]
    fn a_poisoned_cache_recovers_instead_of_panicking() {
        let dir = std::env::temp_dir().join(format!("kuro-md5-poison-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = Md5Cache::load(&dir);
        cache.store("a.pak", 1, 1, "deadbeef");

        let proxy = cache.clone();
        let _ = std::thread::spawn(move || {
            let _guard = proxy.lock();
            panic!("die holding the lock");
        })
        .join();

        // Recovered, and the pre-panic entry is gone rather than trusted.
        assert_eq!(cache.lookup("a.pak", 1, 1), None);
        // Recording works again, and the poison flag is not left set.
        cache.store("b.pak", 2, 2, "cafebabe");
        assert_eq!(cache.lookup("b.pak", 2, 2).as_deref(), Some("cafebabe"));
        assert_eq!(cache.lookup("a.pak", 1, 1), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
