//! Bounded in-memory block cache, generic over the stored value.
//!
//! The cache holds the last N blocks the relayer has accepted, indexed
//! by both height and block id, so two consumers can ask for the same
//! block without racing back to upstream RPC:
//!
//! - `Fetch.Block(BlockNumber)` — random-access lookup by height.
//! - `Stream.Blocks(start_block_num=H)` (when wired) — backfill walks
//!   the cache forward from `H` and falls through to upstream only on
//!   a cache miss for the head-side gap.
//!
//! Generic over `T` so `lightcycle-store` doesn't pull in
//! `lightcycle-relayer`. Callers (typically the relayer) instantiate
//! `BlockCache<BufferedBlock>`; the firehose Fetch path threads the
//! same `Arc<RwLock<…>>` for read access.
//!
//! ## Eviction
//!
//! Bounded by `capacity`; eviction strategy is "drop the lowest
//! height first." The use case is "cache the last N blocks for
//! random-access by recent consumers" — height-LRU is correct because
//! consumer demand drops sharply with age (live tail asks for height
//! H, recent backfill walks from H-1000ish forward, ancient backfill
//! is a separate persistent-store concern that we don't yet
//! implement). Pure LRU would be marginally more responsive to the
//! "consumer asks for an old block twice in a row" pattern, but it
//! costs an extra allocation per access and the win is small for a
//! cache this short.
//!
//! ## Thread-safety
//!
//! `BlockCache<T>` is plain `Send + Sync` when `T` is — interior
//! mutability is the caller's call. The expected shared form is
//! `Arc<RwLock<BlockCache<T>>>`; [`SharedBlockCache`] is the type
//! alias and [`new_shared`] is the helper that allocates one. Reads
//! greatly outnumber writes (one writer = the relayer; many readers
//! = the firehose handlers), so a `tokio::sync::RwLock` is the
//! intended wrapper, but we don't bake that choice into the type so
//! sync code paths can use a `parking_lot::RwLock` instead.
//!
//! ## What the cache is NOT
//!
//! - Not durable: process restart loses everything. Persistent
//!   block storage is a separate concern (the redb-backed cursor
//!   store is the first persistent surface; a finalized-block
//!   archive lands later).
//! - Not the reorg-engine's working set: the relayer maintains its
//!   own canonical `VecDeque<BufferedBlock>` for reorg replay. This
//!   cache is downstream of that — fed on every successful emission,
//!   read by the firehose Fetch path.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tokio::sync::RwLock;

use lightcycle_types::{BlockHeight, BlockId};

/// Bounded in-memory cache of recent blocks, indexed by height and id.
///
/// `T` is the stored value (typically `lightcycle_relayer::BufferedBlock`,
/// but the cache doesn't care). Generic so `lightcycle-store` stays free
/// of upstream-crate dependencies.
#[derive(Debug)]
pub struct BlockCache<T> {
    by_height: BTreeMap<BlockHeight, (BlockId, T)>,
    by_id: HashMap<BlockId, BlockHeight>,
    capacity: usize,
}

impl<T: Clone> BlockCache<T> {
    /// Build a cache bounded to `capacity` entries. Capacity must be
    /// positive (a zero-capacity cache is degenerate; treat it as a
    /// caller bug).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "BlockCache capacity must be positive");
        Self {
            by_height: BTreeMap::new(),
            by_id: HashMap::with_capacity(capacity),
            capacity,
        }
    }

    /// Insert a block, evicting the lowest-height entry if we're at
    /// capacity. If a block with the same `id` is already present,
    /// the existing entry is replaced (this happens during reorg —
    /// the engine assigns a new fork_id to the same height + new id;
    /// for the same id, replacement is a no-op semantically but lets
    /// callers update the value without thinking about it).
    ///
    /// Returns `true` if an eviction occurred. Surfaced for the
    /// metrics hook in `RelayerService`.
    pub fn insert(&mut self, height: BlockHeight, id: BlockId, value: T) -> bool {
        // If id is already mapped (same block re-inserted), drop the
        // prior height entry when the height differs (shouldn't happen
        // with sane callers, but cheap to be safe).
        if let Some(prior_height) = self.by_id.remove(&id) {
            if prior_height != height {
                self.by_height.remove(&prior_height);
            }
        }
        // If a different block already occupies this height (the
        // reorg case: same height, new id), drop its by_id mapping
        // so `get_by_id(old_id)` correctly returns None after the
        // replacement. BTreeMap::insert returns the displaced entry.
        if let Some((displaced_id, _)) = self.by_height.insert(height, (id, value)) {
            if displaced_id != id {
                self.by_id.remove(&displaced_id);
            }
        }
        self.by_id.insert(id, height);

        let mut evicted = false;
        while self.by_height.len() > self.capacity {
            // BTreeMap::pop_first is O(log n); evict lowest height.
            if let Some((evicted_height, (evicted_id, _))) = self.by_height.pop_first() {
                self.by_id.remove(&evicted_id);
                evicted = true;
                metrics::counter!(
                    "lightcycle_store_cache_evictions_total",
                    "reason" => "capacity"
                )
                .increment(1);
                tracing::trace!(
                    height = evicted_height,
                    "block cache evicted lowest-height entry"
                );
            } else {
                break;
            }
        }
        evicted
    }

    /// Look up by height. Clones the stored value — `T: Clone` is the
    /// price of a generic cache that doesn't force callers to hold
    /// the lock across deserialization.
    pub fn get_by_height(&self, height: BlockHeight) -> Option<(BlockId, T)> {
        self.by_height.get(&height).cloned()
    }

    /// Look up by block id (paying for the second index). Returns
    /// the height alongside so callers can confirm what they got.
    pub fn get_by_id(&self, id: BlockId) -> Option<(BlockHeight, T)> {
        let height = *self.by_id.get(&id)?;
        let (_, value) = self.by_height.get(&height)?;
        Some((height, value.clone()))
    }

    /// Drop every entry strictly below `height`. Returns the number
    /// of entries removed. Used after a finalized-tier transition so
    /// the cache stops paying for blocks that downstream consumers
    /// will resume against persistent storage instead of memory.
    pub fn prune_below(&mut self, height: BlockHeight) -> usize {
        let mut removed = 0usize;
        while let Some((&first, _)) = self.by_height.iter().next() {
            if first >= height {
                break;
            }
            if let Some((_, (id, _))) = self.by_height.pop_first() {
                self.by_id.remove(&id);
                removed += 1;
            }
        }
        removed
    }

    /// Number of cached blocks.
    pub fn len(&self) -> usize {
        self.by_height.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_height.is_empty()
    }

    /// Configured capacity (for diagnostics / metrics).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Lowest height currently cached. None if empty.
    pub fn min_height(&self) -> Option<BlockHeight> {
        self.by_height.keys().next().copied()
    }

    /// Highest height currently cached. None if empty.
    pub fn max_height(&self) -> Option<BlockHeight> {
        self.by_height.keys().next_back().copied()
    }
}

/// Convenience alias for the share-across-tasks form.
pub type SharedBlockCache<T> = Arc<RwLock<BlockCache<T>>>;

/// Build a [`SharedBlockCache`] of the given capacity. The relayer
/// instantiates one of these and clones the `Arc` into both itself
/// (writes) and the firehose Fetch handler (reads).
pub fn new_shared<T: Clone>(capacity: usize) -> SharedBlockCache<T> {
    Arc::new(RwLock::new(BlockCache::new(capacity)))
}

/// Describe the cache metrics so they show up in Prometheus output
/// before the first eviction.
pub fn describe_cache_metrics() {
    metrics::describe_counter!(
        "lightcycle_store_cache_evictions_total",
        "Block cache evictions (lowest-height-first under capacity pressure). \
         Sustained nonzero rate is normal at steady state — cache is bounded by design."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> BlockId {
        BlockId([byte; 32])
    }

    #[test]
    fn insert_and_get_by_height() {
        let mut c = BlockCache::<u32>::new(4);
        c.insert(100, id(1), 100);
        c.insert(101, id(2), 101);
        assert_eq!(c.get_by_height(100), Some((id(1), 100)));
        assert_eq!(c.get_by_height(101), Some((id(2), 101)));
        assert_eq!(c.get_by_height(102), None);
    }

    #[test]
    fn insert_and_get_by_id() {
        let mut c = BlockCache::<u32>::new(4);
        c.insert(100, id(1), 100);
        assert_eq!(c.get_by_id(id(1)), Some((100, 100)));
        assert_eq!(c.get_by_id(id(2)), None);
    }

    #[test]
    fn evicts_lowest_height_at_capacity() {
        let mut c = BlockCache::<u32>::new(2);
        assert!(!c.insert(100, id(1), 100));
        assert!(!c.insert(101, id(2), 101));
        // At cap; next insert evicts height=100 (the lowest).
        assert!(c.insert(102, id(3), 102));
        assert_eq!(c.len(), 2);
        assert_eq!(c.get_by_height(100), None);
        assert_eq!(c.get_by_id(id(1)), None);
        assert_eq!(c.get_by_height(101), Some((id(2), 101)));
        assert_eq!(c.get_by_height(102), Some((id(3), 102)));
    }

    #[test]
    fn prune_below_removes_strictly_lower_heights() {
        let mut c = BlockCache::<u32>::new(8);
        for h in 100..=104 {
            c.insert(h, id(h as u8), h as u32);
        }
        let removed = c.prune_below(102);
        assert_eq!(removed, 2);
        assert_eq!(c.len(), 3);
        assert_eq!(c.get_by_height(101), None);
        assert_eq!(c.get_by_height(102), Some((id(102), 102)));
    }

    #[test]
    fn prune_below_zero_is_noop() {
        let mut c = BlockCache::<u32>::new(4);
        c.insert(100, id(1), 100);
        let removed = c.prune_below(0);
        assert_eq!(removed, 0);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn replacing_same_id_at_same_height_is_idempotent() {
        let mut c = BlockCache::<u32>::new(4);
        c.insert(100, id(1), 100);
        c.insert(100, id(1), 999);
        assert_eq!(c.len(), 1);
        // Value updated to 999.
        assert_eq!(c.get_by_height(100), Some((id(1), 999)));
    }

    #[test]
    fn reorg_replaces_height_and_drops_old_id() {
        // Reorg case: height 100 first held block A, then a different
        // block B at the same height. Old by_id mapping must be dropped
        // so a stale lookup against A returns None.
        let mut c = BlockCache::<u32>::new(4);
        c.insert(100, id(0xa), 100);
        c.insert(100, id(0xb), 200);
        assert_eq!(c.len(), 1);
        assert_eq!(c.get_by_height(100), Some((id(0xb), 200)));
        // Critical: stale id lookup should NOT return B's value.
        assert_eq!(c.get_by_id(id(0xa)), None);
        assert_eq!(c.get_by_id(id(0xb)), Some((100, 200)));
    }

    #[test]
    fn min_max_height_track_inserts_and_evictions() {
        let mut c = BlockCache::<u32>::new(2);
        assert_eq!(c.min_height(), None);
        c.insert(100, id(1), 100);
        c.insert(101, id(2), 101);
        assert_eq!(c.min_height(), Some(100));
        assert_eq!(c.max_height(), Some(101));
        c.insert(102, id(3), 102);
        // 100 evicted.
        assert_eq!(c.min_height(), Some(101));
        assert_eq!(c.max_height(), Some(102));
    }

    #[tokio::test]
    async fn shared_cache_round_trips_through_arc_rwlock() {
        let cache: SharedBlockCache<u32> = new_shared(4);
        cache.write().await.insert(100, id(1), 42);
        let got = cache.read().await.get_by_height(100);
        assert_eq!(got, Some((id(1), 42)));
    }
}
