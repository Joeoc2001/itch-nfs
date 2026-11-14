//! A local disk cache of files from the archive directory.
//!
//! The archive is expected to be slow or costly to read (an object store
//! mount), and zips are read with one seek per asset. So the first time a
//! file is needed it is copied here whole, front to back, checked against the
//! SHA-256 recorded for it, and every read after that uses the local copy.
//!
//! Copies are named by their SHA-256, so the directory survives restarts and
//! cannot serve one file's bytes as another's. The least recently used copies
//! are deleted once the total passes the budget, but never the most recently
//! used one: a file larger than the whole budget is kept until another file
//! is used, so the cache holds at most one file beyond its budget.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

use crate::cache::KeyedLocks;
use crate::store::copy_hashed;

#[derive(Clone)]
pub struct DiskCache(Arc<Inner>);

struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    state: Mutex<State>,
    locks: KeyedLocks<String>,
    copies: AtomicU64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    bytes: u64,
    /// A logical clock: each use gets the next value.
    clock: u64,
}

struct Entry {
    size: u64,
    last_used: u64,
    /// Live [`Cached`] handles. A pinned copy is never evicted.
    pins: usize,
}

/// A file in the cache. It stays on disk for as long as this is alive.
pub struct Cached {
    path: PathBuf,
    key: String,
    inner: Arc<Inner>,
}

impl Cached {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Cached {
    fn drop(&mut self) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(e) = st.entries.get_mut(&self.key) {
            e.pins = e.pins.saturating_sub(1);
        }
        // Whatever could not be evicted while this was pinned can go now.
        self.inner.evict(&mut st);
    }
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Inner {
    /// Delete least recently used copies until the budget is met. Pinned
    /// copies are spared, so the total can exceed the budget while they are in
    /// use. So is the most recently used copy, pinned or not: callers hold a
    /// copy for one read at a time, and a file larger than the whole budget
    /// would otherwise be deleted after each read and copied out of the
    /// archive again for the next.
    fn evict(&self, st: &mut State) {
        while st.bytes > self.max_bytes {
            let newest = st.entries.values().map(|e| e.last_used).max();
            let victim = st
                .entries
                .iter()
                .filter(|(_, e)| e.pins == 0 && Some(e.last_used) != newest)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            let Some(key) = victim else {
                return;
            };
            let entry = st.entries.remove(&key).unwrap();
            st.bytes -= entry.size;
            debug!("evicting {key} ({} bytes) from the disk cache", entry.size);
            if let Err(e) = std::fs::remove_file(self.dir.join(&key))
                && e.kind() != std::io::ErrorKind::NotFound
            {
                warn!("could not remove {key} from the disk cache: {e}");
            }
        }
    }

    /// Pin the copy named `key`, if there is one.
    fn pin(self: &Arc<Self>, key: &str) -> Option<Cached> {
        let mut st = self.state.lock().unwrap();
        st.clock += 1;
        let clock = st.clock;
        let path = self.dir.join(key);
        let entry = st.entries.get_mut(key)?;
        if entry.pins == 0 && !path.exists() {
            // Deleted behind our back: forget it, so that it is copied again.
            let size = entry.size;
            st.entries.remove(key);
            st.bytes -= size;
            return None;
        }
        entry.last_used = clock;
        entry.pins += 1;
        Some(Cached {
            path,
            key: key.to_string(),
            inner: self.clone(),
        })
    }
}

impl DiskCache {
    /// Open (or create) a cache in `dir`, adopting the copies already in it.
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).with_context(|| format!("list {}", dir.display()))? {
            let entry = entry?;
            let meta = entry.metadata()?;
            match entry.file_name().into_string() {
                Ok(name) if meta.is_file() && is_sha256(&name) => {
                    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    found.push((mtime, name, meta.len()));
                }
                // Interrupted copies, and anything that is not ours.
                _ if meta.is_file() => {
                    let _ = std::fs::remove_file(entry.path());
                }
                _ => {}
            }
        }
        // Oldest first, so that the oldest are the first to be evicted.
        found.sort();
        let mut st = State::default();
        for (_, name, size) in found {
            st.clock += 1;
            st.bytes += size;
            st.entries.insert(
                name,
                Entry {
                    size,
                    last_used: st.clock,
                    pins: 0,
                },
            );
        }
        let inner = Inner {
            dir,
            max_bytes,
            state: Mutex::new(State::default()),
            locks: KeyedLocks::default(),
            copies: AtomicU64::new(0),
        };
        inner.evict(&mut st);
        *inner.state.lock().unwrap() = st;
        Ok(Self(Arc::new(inner)))
    }

    /// The local copy of `src`, which must be `size` bytes with the given
    /// SHA-256. Copies it in first if it is not here yet; a copy that does not
    /// match is discarded and reported as an error.
    pub async fn get(&self, src: &Path, size: u64, sha256: &str) -> Result<Cached> {
        if !is_sha256(sha256) {
            bail!("{}: {sha256:?} is not a SHA-256", src.display());
        }
        if let Some(c) = self.0.pin(sha256) {
            return Ok(c);
        }
        // One copy at a time per file; whoever waited finds it done.
        let _guard = self.0.locks.lock(sha256.to_string()).await;
        if let Some(c) = self.0.pin(sha256) {
            return Ok(c);
        }
        let dest = self.0.dir.join(sha256);
        let part = self.0.dir.join(format!("{sha256}.part"));
        let (from, to, expected) = (src.to_path_buf(), part.clone(), sha256.to_string());
        let copied = tokio::task::spawn_blocking(move || -> Result<()> {
            let (n, actual) = copy_hashed(&from, &to)?;
            if n != size || actual != expected {
                bail!(
                    "{} is corrupt: {n} bytes with SHA-256 {actual}, expected {size} bytes with {expected}",
                    from.display()
                );
            }
            Ok(())
        })
        .await;
        self.0.copies.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = copied.map_err(anyhow::Error::from).and_then(|r| r) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &dest).with_context(|| format!("rename to {}", dest.display()))?;

        let mut st = self.0.state.lock().unwrap();
        st.clock += 1;
        let clock = st.clock;
        st.bytes += size;
        st.entries.insert(
            sha256.to_string(),
            Entry {
                size,
                last_used: clock,
                pins: 1,
            },
        );
        // The new copy is the most recently used, so this spares it.
        self.0.evict(&mut st);
        if size > self.0.max_bytes {
            warn!(
                "{} ({size} bytes) is larger than the whole disk cache budget ({} bytes): \
                 its copy is kept until another file is read, then copied from the archive again",
                src.display(),
                self.0.max_bytes
            );
        }
        Ok(Cached {
            path: dest,
            key: sha256.to_string(),
            inner: self.0.clone(),
        })
    }

    /// How many times a file has been read from the archive (successfully or not).
    pub fn copies(&self) -> u64 {
        self.0.copies.load(Ordering::SeqCst)
    }

    /// Total size of the copies held.
    pub fn bytes(&self) -> u64 {
        self.0.state.lock().unwrap().bytes
    }

    pub fn len(&self) -> usize {
        self.0.state.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sha256_hex;

    /// A source file of `len` bytes of `fill`; returns (path, size, sha256).
    fn source(dir: &Path, name: &str, fill: u8, len: usize) -> (PathBuf, u64, String) {
        let data = vec![fill; len];
        let path = dir.join(name);
        std::fs::write(&path, &data).unwrap();
        (path, len as u64, sha256_hex(&data))
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn copies_once_and_then_serves_locally() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let c = DiskCache::new(cache.path(), 1000).unwrap();
        let (a, size, sha) = source(src.path(), "a", 1, 100);
        let first = c.get(&a, size, &sha).await.unwrap();
        assert_eq!(std::fs::read(first.path()).unwrap(), vec![1u8; 100]);
        assert_eq!(c.copies(), 1);
        drop(first);
        // The source is not needed again.
        std::fs::remove_file(&a).unwrap();
        let again = c.get(&a, size, &sha).await.unwrap();
        assert_eq!(std::fs::read(again.path()).unwrap(), vec![1u8; 100]);
        assert_eq!(c.copies(), 1);
        assert_eq!(c.bytes(), 100);
    }

    #[tokio::test]
    async fn concurrent_requests_copy_once() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let c = DiskCache::new(cache.path(), 1 << 30).unwrap();
        let (a, size, sha) = source(src.path(), "a", 1, 1 << 20);
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (c, a, sha) = (c.clone(), a.clone(), sha.clone());
                tokio::spawn(async move { c.get(&a, size, &sha).await.map(|_| ()) })
            })
            .collect();
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(c.copies(), 1);
    }

    #[tokio::test]
    async fn evicts_the_least_recently_used() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let c = DiskCache::new(cache.path(), 250).unwrap();
        let (a, _, sha_a) = source(src.path(), "a", 1, 100);
        let (b, _, sha_b) = source(src.path(), "b", 2, 100);
        let (d, _, sha_d) = source(src.path(), "d", 3, 100);
        c.get(&a, 100, &sha_a).await.unwrap();
        c.get(&b, 100, &sha_b).await.unwrap();
        // Touch a, so that b is now the least recently used.
        c.get(&a, 100, &sha_a).await.unwrap();
        assert_eq!(c.copies(), 2);
        c.get(&d, 100, &sha_d).await.unwrap();
        assert_eq!(c.bytes(), 200);
        let mut expected = vec![sha_a.clone(), sha_d.clone()];
        expected.sort();
        assert_eq!(names(cache.path()), expected, "b was evicted");
        // b comes back with another copy, pushing out a.
        c.get(&b, 100, &sha_b).await.unwrap();
        assert_eq!(c.copies(), 4);
        let mut expected = vec![sha_b, sha_d];
        expected.sort();
        assert_eq!(names(cache.path()), expected);
    }

    #[tokio::test]
    async fn copies_in_use_are_not_evicted() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let c = DiskCache::new(cache.path(), 150).unwrap();
        let (a, _, sha_a) = source(src.path(), "a", 1, 100);
        let (b, _, sha_b) = source(src.path(), "b", 2, 100);
        let held = c.get(&a, 100, &sha_a).await.unwrap();
        let other = c.get(&b, 100, &sha_b).await.unwrap();
        assert!(held.path().exists() && other.path().exists());
        assert_eq!(c.bytes(), 200, "over budget while both are in use");
        drop(held);
        assert_eq!(c.bytes(), 100, "evicted as soon as it is released");
        assert_eq!(names(cache.path()), [sha_b]);
        // A file larger than the whole budget is still served.
        drop(other);
        let (big, _, sha_big) = source(src.path(), "big", 4, 400);
        let big = c.get(&big, 400, &sha_big).await.unwrap();
        assert_eq!(std::fs::metadata(big.path()).unwrap().len(), 400);
        assert_eq!(names(cache.path()), [sha_big]);
    }

    #[tokio::test]
    async fn a_file_over_the_budget_is_copied_once_and_kept_until_another_is_used() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let c = DiskCache::new(cache.path(), 100).unwrap();
        let (big, _, sha_big) = source(src.path(), "big", 1, 150);
        // Each handle is dropped before the next read, as the server does.
        for _ in 0..5 {
            let got = c.get(&big, 150, &sha_big).await.unwrap();
            assert_eq!(std::fs::read(got.path()).unwrap(), vec![1u8; 150]);
        }
        assert_eq!(c.copies(), 1, "read from the archive once");
        assert_eq!(c.bytes(), 150, "one file beyond the budget");
        assert_eq!(names(cache.path()), std::slice::from_ref(&sha_big));
        // It is also kept across a restart.
        let c = DiskCache::new(cache.path(), 100).unwrap();
        c.get(&big, 150, &sha_big).await.unwrap();
        assert_eq!(c.copies(), 0);
        // Another file needs the space: the big one goes, even though the
        // small one fits the budget by itself.
        let (small, _, sha_small) = source(src.path(), "small", 2, 50);
        c.get(&small, 50, &sha_small).await.unwrap();
        assert_eq!(names(cache.path()), [sha_small]);
        assert_eq!(c.bytes(), 50);
        // Two files over the budget never stay together.
        let (huge, _, sha_huge) = source(src.path(), "huge", 3, 300);
        c.get(&big, 150, &sha_big).await.unwrap();
        c.get(&huge, 300, &sha_huge).await.unwrap();
        assert_eq!(names(cache.path()), [sha_huge]);
        assert_eq!(c.bytes(), 300);
    }

    #[tokio::test]
    async fn a_checksum_mismatch_is_an_error_and_leaves_nothing() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let c = DiskCache::new(cache.path(), 1000).unwrap();
        let (a, size, sha) = source(src.path(), "a", 1, 100);
        // Corrupted in the archive: same size, different bytes.
        std::fs::write(&a, vec![9u8; 100]).unwrap();
        let err = c.get(&a, size, &sha).await.err().unwrap();
        assert!(err.to_string().contains("corrupt"), "{err:#}");
        assert!(names(cache.path()).is_empty());
        assert!(c.is_empty());
        // Truncated.
        std::fs::write(&a, vec![1u8; 50]).unwrap();
        assert!(c.get(&a, size, &sha).await.is_err());
        // A checksum that is not one must not become a file name.
        assert!(c.get(&a, size, "../../etc/passwd").await.is_err());
        assert!(names(cache.path()).is_empty());
        // Once the archive is right again, so is the cache.
        std::fs::write(&a, vec![1u8; 100]).unwrap();
        c.get(&a, size, &sha).await.unwrap();
        assert_eq!(names(cache.path()), [sha]);
    }

    #[tokio::test]
    async fn survives_a_restart_and_a_wiped_directory() {
        let (src, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, _, sha_a) = source(src.path(), "a", 1, 100);
        let (b, _, sha_b) = source(src.path(), "b", 2, 100);
        {
            let c = DiskCache::new(cache.path(), 1000).unwrap();
            c.get(&a, 100, &sha_a).await.unwrap();
            c.get(&b, 100, &sha_b).await.unwrap();
        }
        std::fs::write(cache.path().join(format!("{sha_a}.part")), b"half").unwrap();
        let c = DiskCache::new(cache.path(), 1000).unwrap();
        assert_eq!(c.len(), 2, "existing copies are adopted");
        assert_eq!(c.bytes(), 200);
        let mut expected = vec![sha_a.clone(), sha_b.clone()];
        expected.sort();
        assert_eq!(
            names(cache.path()),
            expected,
            "interrupted copies are removed"
        );
        c.get(&a, 100, &sha_a).await.unwrap();
        assert_eq!(c.copies(), 0);
        // Someone empties the directory under a running server.
        std::fs::remove_file(cache.path().join(&sha_a)).unwrap();
        let again = c.get(&a, 100, &sha_a).await.unwrap();
        assert_eq!(std::fs::read(again.path()).unwrap(), vec![1u8; 100]);
        assert_eq!(c.copies(), 1);
        assert_eq!(c.bytes(), 200);
        // A smaller budget on restart evicts down to it.
        drop(again);
        let c = DiskCache::new(cache.path(), 100).unwrap();
        assert_eq!(c.len(), 1);
    }
}
