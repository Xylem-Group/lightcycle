//! Block oracle abstraction for `Fetch.Block`.
//!
//! `Stream.Blocks` reads from the live broadcast hub. `Fetch.Block` is a
//! point-in-time, request-driven RPC — the firehose has no live state for
//! a height the operator asks about. The oracle is the indirection that
//! bridges the gap.
//!
//! ## Implementations
//!
//! - The CLI builds a base [`BlockOracle`] backed by a dedicated
//!   `GrpcSource` connection (separate from the relayer's source, which
//!   is owned by the live-tail pipeline). On each `Fetch.Block` call
//!   the upstream `Wallet.GetBlockByNum` RPC fires, the result is
//!   decoded, and the canonical envelope is returned.
//! - [`CachingBlockOracle`] decorates any inner oracle with a read-
//!   through `lightcycle_store::SharedBlockCache`. The relayer feeds
//!   the cache on every `Output::New`/`Output::Undo`; cache hits
//!   short-circuit the upstream RPC. Finality on cache hits is
//!   recomputed fresh from the chain solidified-head watch channel
//!   (cached blocks must NOT carry stale finality — a block that was
//!   `Seen` at cache time will be `Finalized` later, and the oracle
//!   must reflect that).
//!
//! Finality stamping on cache miss is the inner oracle's responsibility
//! (the CLI's `GrpcBlockOracle` does its own `WalletSolidity.GetNowBlock`
//! round-trip per request). On hit, the decorator computes finality from
//! the watched solidified head — one atomic load, no I/O.
//!
//! Why a trait and not a concrete struct: the firehose crate must stay
//! free of source-side dependencies (`tonic` channels, RPC error types).
//! Implementations live in the CLI (`GrpcBlockOracle`) plus the cache
//! decorator here; tests substitute in-memory fakes.

use std::sync::Arc;

use async_trait::async_trait;
use lightcycle_relayer::BufferedBlock;
use lightcycle_store::SharedBlockCache;
use lightcycle_types::{BlockFinality, BlockHeight};
use tokio::sync::watch;

/// What the `Fetch.Block` service needs from anything that can produce a
/// canonical block: the block in the same shape `encode_block` accepts,
/// plus the finality envelope to stamp on the response.
///
/// `Ok(None)` is the not-found state — the upstream chain doesn't have a
/// block at that height (likely too high). `Err(_)` is a server-side
/// failure (RPC, decode); the service surfaces it as `Status::internal`.
#[async_trait]
pub trait BlockOracle: Send + Sync + 'static {
    async fn fetch_block_by_number(
        &self,
        height: u64,
    ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>>;
}

/// Convenience alias — the `Arc<dyn BlockOracle>` shape the firehose
/// `serve` function takes. Keeps call sites readable.
pub type SharedBlockOracle = Arc<dyn BlockOracle>;

/// Describe the cache-related metrics so they show up in Prometheus
/// output before the first request.
pub fn describe_oracle_metrics() {
    metrics::describe_counter!(
        "lightcycle_firehose_fetch_cache_total",
        "Fetch.Block cache probes, labelled by result (hit|miss)."
    );
}

/// Read-through cache decorator for any [`BlockOracle`].
///
/// Holds a `SharedBlockCache<BufferedBlock>` (the same Arc the relayer
/// writes into) plus a `watch::Receiver<Option<BlockHeight>>` for the
/// chain solidified head. On `fetch_block_by_number(h)`:
///
/// 1. Probe the cache. On hit, recompute finality from the *current*
///    solidified head (cached blocks never carry stored finality —
///    the tier ages monotonically as the head advances and the cache
///    can't "update" rows in place without doing extra work).
/// 2. On miss, delegate to the inner oracle. On a successful inner
///    fetch, populate the cache with the buffered block (finality
///    stays out of the cache for the same reason).
///
/// **`has_buffered_descendant=false` always** — `Fetch.Block` doesn't
/// know about consumer-side buffer state, and the engine's
/// `Confirmed` tier requires "we've seen a child built on top." For a
/// random-access query the honest answers are `Seen`, `Confirmed` (if
/// the engine fed us this block already and we know it has a child by
/// height), or `Finalized` (if at-or-below the chain's solidified
/// head). We default to `false` here to avoid lying — callers who
/// want `Confirmed` should subscribe to `Stream.Blocks`.
///
/// ## Invariant
///
/// The cache MUST be the same `Arc` the relayer is writing into for
/// the cache to be useful — otherwise reads always miss and this
/// decorator is just overhead. CLI is responsible for threading.
#[allow(missing_debug_implementations)]
pub struct CachingBlockOracle<I: BlockOracle> {
    cache: SharedBlockCache<BufferedBlock>,
    inner: I,
    solidified_head: watch::Receiver<Option<BlockHeight>>,
}

impl<I: BlockOracle> CachingBlockOracle<I> {
    pub fn new(
        cache: SharedBlockCache<BufferedBlock>,
        inner: I,
        solidified_head: watch::Receiver<Option<BlockHeight>>,
    ) -> Self {
        Self {
            cache,
            inner,
            solidified_head,
        }
    }
}

#[async_trait]
impl<I: BlockOracle> BlockOracle for CachingBlockOracle<I> {
    async fn fetch_block_by_number(
        &self,
        height: u64,
    ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>> {
        // Cache probe under read lock. Drop the guard before any
        // .await so we don't hold across the upstream RPC on miss.
        let cached = {
            let guard = self.cache.read().await;
            guard.get_by_height(height)
        };
        if let Some((_, buffered)) = cached {
            metrics::counter!(
                "lightcycle_firehose_fetch_cache_total",
                "result" => "hit"
            )
            .increment(1);
            let head = *self.solidified_head.borrow();
            let finality = BlockFinality::for_block(height, head, false);
            return Ok(Some((buffered, finality)));
        }

        metrics::counter!(
            "lightcycle_firehose_fetch_cache_total",
            "result" => "miss"
        )
        .increment(1);
        let inner_result = self.inner.fetch_block_by_number(height).await?;
        if let Some((buffered, _)) = inner_result.as_ref() {
            // Populate the cache so a subsequent request for the same
            // height short-circuits. Drop the lock as soon as the
            // insert returns.
            let mut guard = self.cache.write().await;
            guard.insert(buffered.height, buffered.block_id, buffered.clone());
        }
        Ok(inner_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_codec::{DecodedBlock, DecodedHeader};
    use lightcycle_types::{Address, BlockId, FinalityTier};

    fn synth_buffered(height: BlockHeight, byte: u8) -> BufferedBlock {
        let id = BlockId([byte; 32]);
        BufferedBlock {
            height,
            block_id: id,
            parent_id: BlockId([byte.wrapping_sub(1); 32]),
            fork_id: 0,
            decoded: DecodedBlock {
                header: DecodedHeader {
                    height,
                    block_id: id,
                    parent_id: BlockId([byte.wrapping_sub(1); 32]),
                    raw_data_hash: [0u8; 32],
                    tx_trie_root: [0u8; 32],
                    timestamp_ms: 0,
                    witness: Address([0x41; 21]),
                    witness_signature: vec![],
                    version: 34,
                },
                transactions: vec![],
            },
            tx_infos: vec![],
        }
    }

    /// Counts inner-oracle invocations so we can assert that cache
    /// hits do NOT call the inner. Plus a sentinel that always
    /// returns one specific block.
    struct CountingInner {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        block: Option<BufferedBlock>,
    }

    #[async_trait]
    impl BlockOracle for CountingInner {
        async fn fetch_block_by_number(
            &self,
            _height: u64,
        ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.block.clone().map(|b| {
                let h = b.height;
                (b, BlockFinality::for_block(h, Some(0), false))
            }))
        }
    }

    #[tokio::test]
    async fn cache_miss_delegates_to_inner_and_populates() {
        let cache = lightcycle_store::new_shared::<BufferedBlock>(8);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let block = synth_buffered(100, 0xa);
        let inner = CountingInner {
            calls: calls.clone(),
            block: Some(block.clone()),
        };
        let (_tx, rx) = watch::channel(None);
        let caching = CachingBlockOracle::new(cache.clone(), inner, rx);

        let r = caching.fetch_block_by_number(100).await.unwrap();
        assert!(r.is_some());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Cache should now hold height=100.
        let cached = cache.read().await.get_by_height(100);
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn cache_hit_does_not_call_inner() {
        let cache = lightcycle_store::new_shared::<BufferedBlock>(8);
        let block = synth_buffered(100, 0xa);
        cache
            .write()
            .await
            .insert(block.height, block.block_id, block.clone());

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = CountingInner {
            calls: calls.clone(),
            block: None,
        };
        let (_tx, rx) = watch::channel(None);
        let caching = CachingBlockOracle::new(cache, inner, rx);

        let r = caching.fetch_block_by_number(100).await.unwrap();
        assert!(r.is_some());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "cache hit must not delegate to inner"
        );
    }

    #[tokio::test]
    async fn cache_hit_recomputes_finality_from_watch() {
        // Block at height 100. Initial head = None → tier=Seen. Then
        // advance the head to 105 → next request must report Finalized.
        let cache = lightcycle_store::new_shared::<BufferedBlock>(8);
        let block = synth_buffered(100, 0xa);
        cache
            .write()
            .await
            .insert(block.height, block.block_id, block.clone());

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = CountingInner {
            calls: calls.clone(),
            block: None,
        };
        let (tx, rx) = watch::channel::<Option<BlockHeight>>(None);
        let caching = CachingBlockOracle::new(cache, inner, rx);

        let (_, finality) = caching.fetch_block_by_number(100).await.unwrap().unwrap();
        assert_eq!(finality.tier, FinalityTier::Seen);

        tx.send(Some(105)).unwrap();
        let (_, finality) = caching.fetch_block_by_number(100).await.unwrap().unwrap();
        assert_eq!(finality.tier, FinalityTier::Finalized);
        assert_eq!(finality.solidified_head, Some(105));
    }

    #[tokio::test]
    async fn cache_miss_with_inner_none_does_not_populate() {
        let cache = lightcycle_store::new_shared::<BufferedBlock>(8);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = CountingInner {
            calls: calls.clone(),
            block: None,
        };
        let (_tx, rx) = watch::channel(None);
        let caching = CachingBlockOracle::new(cache.clone(), inner, rx);

        let r = caching.fetch_block_by_number(999).await.unwrap();
        assert!(r.is_none());
        assert_eq!(cache.read().await.len(), 0);
    }
}
