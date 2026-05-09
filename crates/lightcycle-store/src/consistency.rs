//! Consistency-horizon SLO + the chain-finality contract for any
//! future cross-replica work.
//!
//! Two pieces:
//!
//! 1. [`ConsistencyHorizonObserver`] — records `seen_at` per block id
//!    when the relayer first surfaces a `tier=Seen` block, then closes
//!    the loop on the `tier=Finalized` transition by observing the
//!    elapsed wall-clock time into the
//!    `lightcycle_store_block_seen_to_finalized_seconds` histogram.
//!    This is the SLO that ADR-0021 mandates for lightcycle: target
//!    p99 ≤ 5s under healthy operation, alert if >5s sustained for
//!    >5 min.
//!
//! 2. [`ConsistencySource`] — the typed contract that any future
//!    cross-replica plumbing must implement. Per ADR-0021 §2 there is
//!    exactly one legal implementation: chain-reported finality
//!    (i.e., the relayer's view of `WalletSolidity.GetNowBlock`). The
//!    only blessed implementor is [`FinalityFromChain`]. A reviewer
//!    proposing a Raft / Paxos / custom-quorum implementation should
//!    be redirected to ADR-0021 — the chain has a finality machine and
//!    we lean on it.

use std::collections::HashMap;
use std::time::Instant;

use lightcycle_types::{BlockHeight, BlockId};

/// Metric name for the SLO histogram. Exposed so tests + the metrics
/// description bootstrap can reference a single canonical string.
pub const BLOCK_SEEN_TO_FINALIZED_SECONDS_METRIC: &str =
    "lightcycle_store_block_seen_to_finalized_seconds";

/// The consistency contract for any future cross-replica work in
/// `lightcycle-store`. ADR-0021 §2: there is one legal implementor
/// (chain-reported finality). Code that proposes a custom replica-
/// agreement protocol must justify why ADR-0021 is wrong; otherwise
/// it should call into [`FinalityFromChain`].
///
/// The trait surfaces *one* operation: "is this height past chain
/// finality?" That's the only question cross-replica reconciliation
/// should ask. Disagreements at heights below this are settled by the
/// chain's own consensus; disagreements above are by definition
/// unresolved and code must NOT fabricate an answer.
pub trait ConsistencySource: Send + Sync + std::fmt::Debug {
    /// Returns true iff `height` is at or below the chain's last-known
    /// solidified head. None means "no chain claim yet" — callers
    /// MUST treat this as "do not reconcile" rather than "treat as
    /// finalized."
    fn is_finalized(&self, height: BlockHeight) -> Option<bool>;

    /// The chain's last-known solidified head, if any. Same caveat:
    /// None means "no chain claim yet."
    fn solidified_head(&self) -> Option<BlockHeight>;
}

/// The blessed [`ConsistencySource`] implementation: chain-reported
/// finality, sourced from the relayer's view of `WalletSolidity.
/// GetNowBlock`. The relayer maintains the value; consumers see a
/// snapshot via this struct.
#[derive(Debug, Clone, Copy, Default)]
pub struct FinalityFromChain {
    head: Option<BlockHeight>,
}

impl FinalityFromChain {
    pub fn new(head: Option<BlockHeight>) -> Self {
        Self { head }
    }
}

impl ConsistencySource for FinalityFromChain {
    fn is_finalized(&self, height: BlockHeight) -> Option<bool> {
        self.head.map(|h| height <= h)
    }
    fn solidified_head(&self) -> Option<BlockHeight> {
        self.head
    }
}

/// Records `block_id → seen_at` on first emission and closes the
/// loop into a histogram observation when the same block id reaches
/// finalization. Designed to be cheap on the hot path: every observe
/// is one hash-map lookup and one optional histogram emit.
///
/// Bounded by `max_pending`: if a block was seen but never finalized
/// (orphan, or solidified-head fetch was stuck for >max_pending blocks),
/// the entry is evicted on the next insert that would otherwise
/// exceed the cap. Eviction is logged but not metered as a failure —
/// orphans are normal.
#[derive(Debug)]
pub struct ConsistencyHorizonObserver {
    seen_at: HashMap<BlockId, (BlockHeight, Instant)>,
    max_pending: usize,
}

impl ConsistencyHorizonObserver {
    /// Build with a cap on the number of in-flight (seen-but-not-yet-
    /// finalized) blocks. 1024 is plenty for TRON's reorg window
    /// (typical max ~30 blocks of unsolidified head + slack for orphans).
    pub fn new(max_pending: usize) -> Self {
        Self {
            seen_at: HashMap::new(),
            max_pending,
        }
    }

    /// Record that a block was first observed (relayer emitted a
    /// `tier=Seen` block). Idempotent: repeat observes for the same
    /// id keep the original `seen_at` (we want to measure the *first*
    /// observation latency, not the most recent).
    pub fn observe_seen(&mut self, id: BlockId, height: BlockHeight) {
        if self.seen_at.contains_key(&id) {
            return;
        }
        if self.seen_at.len() >= self.max_pending {
            // Evict the oldest entry. HashMap iteration order is not
            // stable so this picks "an entry"; for SLO measurement
            // that's fine — we're trading freshness for boundedness.
            if let Some(victim) = self.seen_at.keys().next().copied() {
                self.seen_at.remove(&victim);
                metrics::counter!(
                    "lightcycle_store_horizon_evictions_total",
                    "reason" => "max_pending"
                )
                .increment(1);
            }
        }
        self.seen_at.insert(id, (height, Instant::now()));
    }

    /// Record that a block has reached `tier=Finalized`. Looks up the
    /// matching `seen_at`, observes the elapsed seconds into the
    /// histogram, and removes the pending entry. No-op if the id is
    /// not pending (it was either evicted, already finalized, or
    /// never observed via this surface).
    pub fn observe_finalized(&mut self, id: BlockId) {
        let Some((height, seen_at)) = self.seen_at.remove(&id) else {
            return;
        };
        let elapsed = seen_at.elapsed().as_secs_f64();
        metrics::histogram!(BLOCK_SEEN_TO_FINALIZED_SECONDS_METRIC).record(elapsed);
        metrics::gauge!("lightcycle_store_finalized_height").set(height as f64);
        tracing::debug!(
            block_id = %hex::display(id.0),
            height,
            elapsed_secs = elapsed,
            "block seen → finalized horizon closed"
        );
    }

    /// Number of pending (seen-but-not-finalized) entries currently
    /// tracked. Test + diagnostic surface.
    pub fn pending_len(&self) -> usize {
        self.seen_at.len()
    }
}

mod hex {
    pub(super) fn display(bytes: [u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

/// Describe the SLO metrics so they appear in Prometheus output
/// (and so the histogram bucket boundaries are properly labeled)
/// even before the first observation.
///
/// Bucket boundaries chosen to span the regime ADR-0021 cares about:
/// healthy operation (well under 5s), the SLO target (5s), and the
/// alert regime (>5s sustained). Beyond ~120s the histogram is
/// less interesting — at that point the alert has long fired.
pub fn describe_metrics() {
    metrics::describe_histogram!(
        BLOCK_SEEN_TO_FINALIZED_SECONDS_METRIC,
        metrics::Unit::Seconds,
        "Wall-clock seconds from block first-seen to block-finalized. \
         ADR-0021 SLO: p99 ≤ 5s under healthy operation; alert if >5s sustained for >5 min."
    );
    metrics::describe_counter!(
        "lightcycle_store_horizon_evictions_total",
        "Pending entries evicted from the consistency-horizon observer (orphan blocks, or \
         max_pending reached). Sustained nonzero rate indicates the chain solidified head is \
         not advancing."
    );
    metrics::describe_gauge!(
        "lightcycle_store_finalized_height",
        "Most recent block height observed crossing tier=Finalized. Should monotonically \
         increase under healthy chain operation."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> BlockId {
        BlockId([byte; 32])
    }

    #[test]
    fn finality_from_chain_reports_unknown_when_no_head() {
        let f = FinalityFromChain::new(None);
        assert_eq!(f.is_finalized(100), None);
        assert_eq!(f.solidified_head(), None);
    }

    #[test]
    fn finality_from_chain_reports_below_head_as_finalized() {
        let f = FinalityFromChain::new(Some(105));
        assert_eq!(f.is_finalized(100), Some(true));
        assert_eq!(f.is_finalized(105), Some(true));
        assert_eq!(f.is_finalized(106), Some(false));
    }

    #[test]
    fn observer_idempotent_on_repeated_seen() {
        let mut o = ConsistencyHorizonObserver::new(16);
        o.observe_seen(id(1), 100);
        o.observe_seen(id(1), 100);
        assert_eq!(o.pending_len(), 1);
    }

    #[test]
    fn observer_drops_pending_on_finalized() {
        let mut o = ConsistencyHorizonObserver::new(16);
        o.observe_seen(id(1), 100);
        o.observe_seen(id(2), 101);
        o.observe_finalized(id(1));
        assert_eq!(o.pending_len(), 1);
        o.observe_finalized(id(2));
        assert_eq!(o.pending_len(), 0);
    }

    #[test]
    fn observer_finalized_for_unseen_is_no_op() {
        let mut o = ConsistencyHorizonObserver::new(16);
        // No matching seen — must not panic, must not crash.
        o.observe_finalized(id(99));
        assert_eq!(o.pending_len(), 0);
    }

    #[test]
    fn observer_evicts_when_at_max_pending() {
        let mut o = ConsistencyHorizonObserver::new(2);
        o.observe_seen(id(1), 100);
        o.observe_seen(id(2), 101);
        // Cap full; next insert evicts one of the existing entries.
        o.observe_seen(id(3), 102);
        assert_eq!(o.pending_len(), 2);
    }
}
