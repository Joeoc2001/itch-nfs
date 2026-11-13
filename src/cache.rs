//! In-memory cache with an idle TTL, a byte budget and random eviction.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;

pub struct MemCache<K> {
    inner: Mutex<Inner<K>>,
    max_bytes: u64,
    ttl: Duration,
}

struct Inner<K> {
    index: HashMap<K, usize>,
    slots: Vec<Slot<K>>,
    bytes: u64,
}

struct Slot<K> {
    key: K,
    data: Bytes,
    last_used: Instant,
}

impl<K: Hash + Eq + Clone> Inner<K> {
    fn remove_at(&mut self, i: usize) {
        let slot = self.slots.swap_remove(i);
        self.index.remove(&slot.key);
        self.bytes -= slot.data.len() as u64;
        if let Some(moved) = self.slots.get(i) {
            self.index.insert(moved.key.clone(), i);
        }
    }
}

impl<K: Hash + Eq + Clone> MemCache<K> {
    pub fn new(max_bytes: u64, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                index: HashMap::new(),
                slots: Vec::new(),
                bytes: 0,
            }),
            max_bytes,
            ttl,
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Returns the entry and resets its TTL, or drops it if it has expired.
    pub fn get(&self, key: &K) -> Option<Bytes> {
        let mut g = self.inner.lock().unwrap();
        let i = *g.index.get(key)?;
        let now = Instant::now();
        if now.duration_since(g.slots[i].last_used) > self.ttl {
            g.remove_at(i);
            return None;
        }
        g.slots[i].last_used = now;
        Some(g.slots[i].data.clone())
    }

    /// Inserts an entry, evicting random entries until it fits. Entries larger
    /// than the whole budget are not cached; returns whether it was stored.
    pub fn insert(&self, key: K, data: Bytes) -> bool {
        let len = data.len() as u64;
        if len > self.max_bytes {
            return false;
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(&i) = g.index.get(&key) {
            g.remove_at(i);
        }
        while g.bytes + len > self.max_bytes {
            let victim = rand::random_range(0..g.slots.len());
            g.remove_at(victim);
        }
        let i = g.slots.len();
        g.index.insert(key.clone(), i);
        g.slots.push(Slot {
            key,
            data,
            last_used: Instant::now(),
        });
        g.bytes += len;
        true
    }

    /// Drops every expired entry.
    pub fn purge_expired(&self) {
        let mut g = self.inner.lock().unwrap();
        let now = Instant::now();
        let mut i = 0;
        while i < g.slots.len() {
            if now.duration_since(g.slots[i].last_used) > self.ttl {
                g.remove_at(i);
            } else {
                i += 1;
            }
        }
    }

    pub fn bytes(&self) -> u64 {
        self.inner.lock().unwrap().bytes
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-key async locks, so that concurrent requests for the same thing
/// (a download, a decompression) do the work once.
pub struct KeyedLocks<K> {
    locks: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}

impl<K: Hash + Eq + Clone> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Hash + Eq + Clone> KeyedLocks<K> {
    pub async fn lock(&self, key: K) -> tokio::sync::OwnedMutexGuard<()> {
        let m = {
            let mut g = self.locks.lock().unwrap();
            // Forget locks nobody holds or waits on.
            g.retain(|_, m| Arc::strong_count(m) > 1);
            g.entry(key).or_default().clone()
        };
        m.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_budget_with_random_eviction() {
        let c = MemCache::new(100, Duration::from_secs(60));
        for k in 0..50u32 {
            assert!(c.insert(k, Bytes::from(vec![0u8; 30])));
            assert!(c.bytes() <= 100);
        }
        assert_eq!(c.len(), 3);
        // The newest entry always survives its own insertion.
        assert!(c.get(&49).is_some());
        // Index stays consistent after swap-removes.
        let live: Vec<u32> = (0..50).filter(|k| c.get(k).is_some()).collect();
        assert_eq!(live.len(), 3);
    }

    #[test]
    fn oversized_entries_are_not_cached() {
        let c = MemCache::new(10, Duration::from_secs(60));
        assert!(!c.insert(1u32, Bytes::from(vec![0u8; 11])));
        assert!(c.is_empty());
    }

    #[test]
    fn entries_expire() {
        let c = MemCache::new(100, Duration::from_millis(20));
        c.insert(1u32, Bytes::from_static(b"abc"));
        c.insert(2u32, Bytes::from_static(b"abc"));
        assert!(c.get(&1).is_some());
        std::thread::sleep(Duration::from_millis(40));
        assert!(c.get(&1).is_none());
        c.purge_expired();
        assert_eq!(c.bytes(), 0);
    }
}
