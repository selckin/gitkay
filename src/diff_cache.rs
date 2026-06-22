//! A small line-budget LRU cache, generic over key and value. Keeps recently
//! viewed diffs (and their highlight spans) so revisiting a commit is instant.
//! Knows nothing of git, egui, or the highlight worker: the caller supplies a
//! per-entry `weight` (we use a diff's line count) and the cache evicts the
//! least-recently-used entries once the total weight exceeds `budget`.

use std::collections::HashMap;
use std::hash::Hash;

struct Entry<V> {
    value: V,
    /// `clock` at the entry's last insert; smallest ⇒ least recently used.
    last_used: u64,
    weight: usize,
}

pub struct DiffCache<K, V> {
    entries: HashMap<K, Entry<V>>,
    total: usize,
    budget: usize,
    clock: u64,
}

impl<K: Clone + Eq + Hash, V> DiffCache<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            total: 0,
            budget,
            clock: 0,
        }
    }

    /// Move an entry out (a cache hit), subtracting its weight. The caller owns
    /// the value and is expected to re-`insert` it when done — the cache never
    /// retains the value the caller is actively using.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.remove(key)?;
        self.total -= entry.weight;
        Some(entry.value)
    }

    /// Whether `key` is cached, without touching LRU recency — a peek, unlike the
    /// move-out `remove`. Used by the prefetch dispatch to skip already-cached
    /// neighbours.
    #[allow(dead_code)] // wired into dispatch_prefetch in the prefetch wiring task
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Insert (or overwrite) an entry of the given weight, then evict the
    /// least-recently-used entries until `total <= budget` — but always keep at
    /// least one entry, so a single value larger than the budget is still cached
    /// (otherwise it would evict itself and the next revisit would miss).
    pub fn insert(&mut self, key: K, value: V, weight: usize) {
        self.clock += 1;
        let entry = Entry {
            value,
            last_used: self.clock,
            weight,
        };
        if let Some(old) = self.entries.insert(key, entry) {
            self.total -= old.weight;
        }
        self.total += weight;
        let mut evicted_n = 0usize;
        let mut evicted_lines = 0usize;
        while self.total > self.budget && self.entries.len() > 1 {
            let lru = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
                .expect("len > 1 ⇒ non-empty");
            if let Some(evicted) = self.entries.remove(&lru) {
                self.total -= evicted.weight;
                evicted_n += 1;
                evicted_lines += evicted.weight;
            }
        }
        if evicted_n > 0 {
            log::debug!(
                "diff cache: insert {weight} lines, evicted {evicted_n} ({evicted_lines} lines) \
                 → {} entries / {} lines (budget {})",
                self.entries.len(),
                self.total,
                self.budget
            );
        } else {
            log::debug!(
                "diff cache: insert {weight} lines → {} entries / {} lines (budget {})",
                self.entries.len(),
                self.total,
                self.budget
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_moves_value_out_and_subtracts() {
        let mut c: DiffCache<u32, &str> = DiffCache::new(100);
        c.insert(1, "a", 10);
        assert_eq!(c.remove(&1), Some("a"));
        assert_eq!(c.remove(&1), None); // gone after the move
    }

    #[test]
    fn evicts_lru_when_over_budget() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        c.insert(1, 1, 20);
        c.insert(2, 2, 20); // total 40 > 30 ⇒ evict key 1 (older)
        assert_eq!(c.remove(&1), None);
        assert_eq!(c.remove(&2), Some(2));
    }

    #[test]
    fn keeps_single_entry_larger_than_budget() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        c.insert(1, 1, 100); // alone and over budget, but len()==1 ⇒ kept
        assert_eq!(c.remove(&1), Some(1));
    }

    #[test]
    fn reinsert_updates_total() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(50);
        c.insert(1, 1, 40);
        c.insert(1, 11, 10); // overwrite: total now 10, not 50
        c.insert(2, 2, 40); // total 50 ⇒ fits, nothing evicted
        assert_eq!(c.remove(&1), Some(11));
        assert_eq!(c.remove(&2), Some(2));
    }

    #[test]
    fn contains_peeks_without_touching_lru() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        c.insert(1, 1, 15);
        c.insert(2, 2, 15); // total 30, key 1 is LRU
        assert!(c.contains(&1));
        assert!(!c.contains(&3));
        // contains(&1) must NOT refresh recency: inserting a third entry over budget
        // must still evict key 1 (the LRU). If contains() bumped it, key 2 would go.
        c.insert(3, 3, 15); // total 45 > 30 → evict LRU
        assert!(!c.contains(&1), "key 1 was LRU; contains() must not have refreshed it");
        assert!(c.contains(&2));
        assert!(c.contains(&3));
    }

    #[test]
    fn reinsert_refreshes_recency() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        c.insert(1, 1, 15);
        c.insert(2, 2, 15); // total 30, both fit
        c.remove(&1); // revisit key 1...
        c.insert(1, 1, 15); // ...and leave again ⇒ key 1 is now MRU
        c.insert(3, 3, 15); // total 45 > 30 ⇒ evict LRU = key 2
        assert_eq!(c.remove(&2), None);
        assert_eq!(c.remove(&1), Some(1));
        assert_eq!(c.remove(&3), Some(3));
    }
}
