//! A small weight-budget LRU cache, generic over key and value. Keeps recently
//! viewed diffs (and their highlight spans) so revisiting a commit is instant.
//! Knows nothing of git, egui, or the highlight worker: the caller supplies a
//! per-entry `weight` (we use a diff's heap bytes, `DiffData::heap_bytes`) and the cache evicts the
//! least-recently-used entries once the total weight exceeds `budget`. What it lets go —
//! evicted, overwritten or purged — it hands back rather than drops, so where a value is
//! freed is the caller's decision (the app frees diffs off the UI thread).

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
        if let Some(entry) = self.entries.remove(key) {
            self.total -= entry.weight;
            log::debug!(
                "diff cache: remove HIT ({} bytes) → {} entries / {} bytes",
                entry.weight,
                self.entries.len(),
                self.total
            );
            Some(entry.value)
        } else {
            log::debug!("diff cache: remove MISS → {} entries", self.entries.len());
            None
        }
    }

    /// The summed weight — bytes — of everything held. For the shutdown timing log,
    /// where the count is what makes the duration mean something.
    pub const fn weight(&self) -> usize {
        self.total
    }

    /// Whether `key` is cached, without touching LRU recency — a peek, unlike the
    /// move-out `remove`. Used by the prefetch dispatch to skip already-cached
    /// neighbours.
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Remove every entry whose key fails `keep`, subtracting the freed weight, and hand
    /// the removed values back (in no particular order) — freeing them is the caller's
    /// decision, not this cache's. Used to purge stale content-keyed entries that share
    /// a sentinel oid but no longer reflect the working tree, so only the current
    /// version stays cached.
    #[must_use = "the removed values are the caller's to free"]
    pub fn retain_keys(&mut self, mut keep: impl FnMut(&K) -> bool) -> Vec<V> {
        let mut removed = Vec::new();
        for (_, e) in self.entries.extract_if(|k, _| !keep(k)) {
            self.total -= e.weight;
            removed.push(e.value);
        }
        removed
    }

    /// Insert (or overwrite) an entry of the given weight, then evict the
    /// least-recently-used entries until `total <= budget` — but always keep at
    /// least one entry, so a single value larger than the budget is still cached
    /// (otherwise it would evict itself and the next revisit would miss).
    ///
    /// Returns what it let go — the overwritten value first, if any, then the evicted
    /// ones, least recently used first — rather than dropping it, so the caller decides
    /// where a value is freed. Empty, and allocation-free, in the common case.
    #[must_use = "the overwritten and evicted values are the caller's to free"]
    pub fn insert(&mut self, key: K, value: V, weight: usize) -> Vec<V> {
        self.clock += 1;
        let entry = Entry {
            value,
            last_used: self.clock,
            weight,
        };
        let mut released = Vec::new();
        if let Some(old) = self.entries.insert(key, entry) {
            self.total -= old.weight;
            released.push(old.value);
        }
        self.total += weight;
        let mut evicted_n = 0usize;
        let mut evicted_bytes = 0usize;
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
                evicted_bytes += evicted.weight;
                released.push(evicted.value);
            }
        }
        if evicted_n > 0 {
            log::debug!(
                "diff cache: insert {weight} bytes, evicted {evicted_n} ({evicted_bytes} bytes) \
                 → {} entries / {} bytes (budget {})",
                self.entries.len(),
                self.total,
                self.budget
            );
        } else {
            log::debug!(
                "diff cache: insert {weight} bytes → {} entries / {} bytes (budget {})",
                self.entries.len(),
                self.total,
                self.budget
            );
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert expecting nothing to be let go — the common case.
    fn put<K: Clone + Eq + Hash, V: std::fmt::Debug>(
        c: &mut DiffCache<K, V>,
        key: K,
        value: V,
        weight: usize,
    ) {
        let released = c.insert(key, value, weight);
        assert!(released.is_empty(), "unexpectedly released {released:?}");
    }

    #[test]
    fn remove_moves_value_out_and_subtracts() {
        let mut c: DiffCache<u32, &str> = DiffCache::new(100);
        put(&mut c, 1, "a", 10);
        assert_eq!(c.remove(&1), Some("a"));
        assert_eq!(c.remove(&1), None); // gone after the move
    }

    #[test]
    fn evicts_lru_when_over_budget() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        put(&mut c, 1, 1, 20);
        // total 40 > 30 ⇒ evict key 1 (older), and hand it back
        assert_eq!(c.insert(2, 2, 20), vec![1]);
        assert_eq!(c.remove(&1), None);
        assert_eq!(c.remove(&2), Some(2));
    }

    #[test]
    fn insert_returns_the_lru_victims_oldest_first() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        put(&mut c, 1, 10, 10);
        put(&mut c, 2, 20, 10);
        put(&mut c, 3, 30, 10); // total 30, all fit
        // total 45 ⇒ evict 1 (35), then 2 (25): two victims, in LRU order, 3 kept
        assert_eq!(c.insert(4, 40, 15), vec![10, 20]);
        assert!(c.contains(&3) && c.contains(&4));
        assert_eq!(c.weight(), 25);
    }

    #[test]
    fn insert_returns_the_overwritten_value_before_its_victims() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        put(&mut c, 1, 10, 10);
        put(&mut c, 2, 20, 10);
        // Overwriting 2 at a heavier weight releases the old 2, and the 35 it brings
        // the total to evicts 1.
        assert_eq!(c.insert(2, 21, 25), vec![20, 10]);
        assert_eq!(c.remove(&2), Some(21));
    }

    #[test]
    fn keeps_single_entry_larger_than_budget() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        put(&mut c, 1, 1, 100); // alone and over budget, but len()==1 ⇒ kept
        assert_eq!(c.remove(&1), Some(1));
    }

    #[test]
    fn reinsert_updates_total() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(50);
        put(&mut c, 1, 1, 40);
        // overwrite: the old value comes back, and total is now 10, not 50
        assert_eq!(c.insert(1, 11, 10), vec![1]);
        put(&mut c, 2, 2, 40); // total 50 ⇒ fits, nothing evicted
        assert_eq!(c.remove(&1), Some(11));
        assert_eq!(c.remove(&2), Some(2));
    }

    #[test]
    fn contains_peeks_without_touching_lru() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        put(&mut c, 1, 1, 15);
        put(&mut c, 2, 2, 15); // total 30, key 1 is LRU
        assert!(c.contains(&1));
        assert!(!c.contains(&3));
        // contains(&1) must NOT refresh recency: inserting a third entry over budget
        // must still evict key 1 (the LRU). If contains() bumped it, key 2 would go.
        assert_eq!(c.insert(3, 3, 15), vec![1]); // total 45 > 30 → evict LRU
        assert!(
            !c.contains(&1),
            "key 1 was LRU; contains() must not have refreshed it"
        );
        assert!(c.contains(&2));
        assert!(c.contains(&3));
    }

    #[test]
    fn retain_keys_drops_matches_and_subtracts_weight() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(100);
        put(&mut c, 10, 1, 15); // stale — to be purged
        put(&mut c, 11, 2, 25); // kept
        put(&mut c, 20, 3, 10); // kept
        // Remove only the entries failing the predicate (here: key 10), handing it back.
        assert_eq!(c.retain_keys(|&k| k != 10), vec![1]);
        assert_eq!(c.remove(&10), None, "stale entry dropped");
        assert_eq!(c.remove(&11), Some(2), "unrelated entry kept");
        assert_eq!(c.remove(&20), Some(3), "current entry kept");
        // total was decremented by the dropped weight: re-inserting up to budget must
        // not evict the survivors.
        put(&mut c, 11, 2, 25);
        put(&mut c, 20, 3, 10);
        put(&mut c, 30, 4, 65); // 25 + 10 + 65 = 100 ⇒ fits exactly, nothing evicted
        assert!(c.contains(&11) && c.contains(&20) && c.contains(&30));
    }

    #[test]
    fn retain_keys_returns_every_removed_value() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(100);
        put(&mut c, 1, 10, 10);
        put(&mut c, 2, 20, 10);
        put(&mut c, 3, 30, 10);
        assert!(
            c.retain_keys(|_| true).is_empty(),
            "nothing removed, nothing returned"
        );
        // A map's order is its own: compare as a set.
        let mut removed = c.retain_keys(|&k| k == 2);
        removed.sort_unstable();
        assert_eq!(removed, vec![10, 30]);
        assert_eq!(c.retain_keys(|_| false), vec![20]);
        assert_eq!(c.weight(), 0);
    }

    #[test]
    fn reinsert_refreshes_recency() {
        let mut c: DiffCache<u32, u32> = DiffCache::new(30);
        put(&mut c, 1, 1, 15);
        put(&mut c, 2, 2, 15); // total 30, both fit
        c.remove(&1); // revisit key 1...
        put(&mut c, 1, 1, 15); // ...and leave again ⇒ key 1 is now MRU
        // total 45 > 30 ⇒ evict LRU = key 2
        assert_eq!(c.insert(3, 3, 15), vec![2]);
        assert_eq!(c.remove(&2), None);
        assert_eq!(c.remove(&1), Some(1));
        assert_eq!(c.remove(&3), Some(3));
    }
}
