//! ReorgEngine: synchronous state machine that converts an in-order
//! stream of decoded blocks into NEW / UNDO / IRREVERSIBLE events.
//!
//! Maintains a bounded VecDeque of canonical blocks plus a HashSet of
//! their ids for O(1) ancestor lookup. When a block arrives:
//!
//!   1. **Linear extension** — `parent_id == canonical tip's id`.
//!      Push, emit `New`. Possibly emit `Irreversible` for the block
//!      that just crossed the finality window. Possibly drop the
//!      oldest block if the buffer is full.
//!
//!   2. **Reorg** — `parent_id != tip` but `parent_id` *is* in the
//!      canonical buffer. The peer is switching us to a fork that
//!      branches at the matched ancestor. Walk back, emit `Undo` for
//!      each block from current tip down to (exclusive) the ancestor,
//!      drop them from canonical, then push the new block and emit
//!      `New`.
//!
//!   3. **Anything else** — `parent_id` not found, or the block
//!      duplicates one we already have, or the height regresses
//!      below the buffer's earliest block. Returns a typed
//!      [`ReorgError`]; nothing is emitted, internal state is
//!      unchanged. The caller (the source-driver loop) decides
//!      whether to log+drop or to backfill.
//!
//! The engine is intentionally pure-sync. Async fan-out, durable
//! cursor stores, and gRPC framing are higher layers' problems.

use std::collections::{HashSet, VecDeque};

use lightcycle_codec::{DecodedBlock, DecodedTxInfo};
use lightcycle_types::{BlockFinality, BlockHeight, BlockId, FinalityTier, Step};
use thiserror::Error;

use crate::cursor::Cursor;

/// Buffered block — the engine's internal record of a block we've
/// accepted. Carries the decoded payload + the fork id assigned by
/// the engine for cursor disambiguation across reorgs.
#[derive(Debug, Clone)]
pub struct BufferedBlock {
    pub height: BlockHeight,
    pub block_id: BlockId,
    pub parent_id: BlockId,
    /// Monotonic id assigned by the engine; increments every time a
    /// block at the same `height` is replaced via reorg. Lets the
    /// firehose disambiguate "block 100 v1" from "block 100 v2 after
    /// undo" in cursor metadata.
    pub fork_id: u32,
    /// The decoded body. Cloned by value into outputs (Arc-wrap
    /// upstream if cloning becomes a hot-path concern).
    pub decoded: DecodedBlock,
    /// Side-channel `TransactionInfo` for the txs in `decoded`,
    /// fetched from java-tron's `getTransactionInfoByBlockNum` and
    /// joined by the service before the engine sees the block.
    /// Order is NOT guaranteed to match `decoded.transactions`; the
    /// firehose encoder joins by tx hash. Empty when the service is
    /// running with `--no-fetch-tx-info` or when the upstream
    /// returned no info (e.g. an empty block, or a node that doesn't
    /// expose the tx-info RPC).
    pub tx_infos: Vec<DecodedTxInfo>,
}

/// One ordered event emitted by the engine. Consumers iterate the
/// output vector in the order returned and apply the steps to their
/// materialized state.
#[derive(Debug, Clone)]
pub enum Output {
    /// New canonical block extended at the head.
    New(StreamableBlock),
    /// Previously-emitted block is being orphaned — consumers must
    /// roll back its effects before applying the subsequent `New`(s).
    Undo(StreamableBlock),
    /// Block has crossed the chain's solidified-head threshold (per
    /// [`BlockFinality::tier`] = [`FinalityTier::Finalized`]).
    /// Consumers can prune any reorg-undo state for it.
    Irreversible(StreamableBlock),
    /// A reorg was observed at this height — the engine performed an
    /// optimistic switch (Undo of `orphaned_tips`, New of `kept_tip`)
    /// in the same `accept` call. This variant is the *audit ledger
    /// entry* for the observation: the engine does not "decide" which
    /// fork is canonical, it records that a competing tip was seen
    /// and that the upstream peer's view drove the optimistic switch.
    /// The chain's solidified head will later finalize one of the two
    /// — surfaced as [`Output::ForkResolved`].
    ///
    /// Implements ADR-0021 §3 ("tag both, wait, log") in the engine.
    ForkObserved {
        observed_at_height: BlockHeight,
        kept_tip: BlockId,
        orphaned_tips: Vec<BlockId>,
    },
    /// The chain's solidified head has advanced past a previously
    /// `ForkObserved` height; this is the chain's resolution of the
    /// fork. We log which tip ended up on the finalized chain (the
    /// one whose ancestry includes the solidified head). Together
    /// with `ForkObserved` this constitutes the structured ledger of
    /// fork events that ADR-0021 §3 mandates.
    ForkResolved {
        observed_at_height: BlockHeight,
        finalized_head: BlockHeight,
        finalized_tip: BlockId,
        orphaned_tips: Vec<BlockId>,
    },
}

/// The unit emitted to consumers. Pairs the decoded block with the
/// step semantics, the resumption cursor, and the chain-reported
/// finality envelope. Per ADR-0021 §1, every record emitted from
/// `lightcycle-firehose` carries its finality tier as a tagged enum.
#[derive(Debug, Clone)]
pub struct StreamableBlock {
    pub step: Step,
    pub block: BufferedBlock,
    pub cursor: Cursor,
    pub finality: BlockFinality,
}

/// Engine configuration. Defaults are TRON-tuned: 30-block buffer
/// covers any plausible reorg depth.
///
/// **Finality is NOT computed here** — it's read from the chain via
/// [`ReorgEngine::set_solidified_head`] (sourced from
/// `WalletSolidity.GetNowBlock`). Per ADR-0021's "the chain has a
/// finality machine; lean on it" discipline. `buffer_window` is now
/// purely a reorg-resistance parameter: how far back the engine can
/// undo without hitting `ParentNotInBuffer`. `finality_depth` is kept
/// as the *minimum buffer width* so the buffer always retains enough
/// history to cover plausible reorg depth, but it does NOT trigger
/// `Irreversible` emission — only the chain-reported solidified head
/// does that.
#[derive(Debug, Clone, Copy)]
pub struct ReorgConfig {
    /// How many canonical blocks to keep in memory. Must be > finality_depth
    /// so reorg-resistance is preserved during steady state.
    pub buffer_window: usize,
    /// Minimum buffer depth before pruning kicks in. Originally a
    /// confirmation-count finality trigger; now a buffer-sizing
    /// constraint only. Kept around because removing it changes the
    /// buffer's pruning guarantee subtly.
    pub finality_depth: usize,
}

impl Default for ReorgConfig {
    fn default() -> Self {
        Self {
            buffer_window: 30,
            finality_depth: 19,
        }
    }
}

/// Internal record of a reorg observation. Held in the engine's
/// fork-history ring until the chain's solidified head advances past
/// `observed_at_height`, at which point the engine emits
/// `Output::ForkResolved` and drops it.
#[derive(Debug, Clone)]
struct ForkObservation {
    observed_at_height: BlockHeight,
    kept_tip: BlockId,
    orphaned_tips: Vec<BlockId>,
}

/// Errors the engine surfaces when a block can't be incorporated.
/// All variants leave engine state unchanged.
#[derive(Debug, Error)]
pub enum ReorgError {
    /// Block claims a parent we don't have. Either upstream skipped
    /// blocks (gap) or the parent fell out of our buffer window
    /// (we're catching up after a long pause). Caller should backfill.
    #[error(
        "parent {} of block at height {height} not in buffer (tip height {tip_height}, window {window})",
        hex::encode(parent.0)
    )]
    ParentNotInBuffer {
        height: BlockHeight,
        parent: BlockId,
        tip_height: BlockHeight,
        window: usize,
    },

    /// Block has a height already covered by an emitted-then-finalized
    /// block. The engine refuses to undo across the finality boundary.
    #[error(
        "block at height {height} would force a reorg below finality (last irreversible {last_irreversible_height})"
    )]
    ReorgBelowFinality {
        height: BlockHeight,
        last_irreversible_height: BlockHeight,
    },

    /// We've already seen this exact block_id. Idempotent re-delivery
    /// is harmless but the engine surfaces it so callers can detect
    /// upstream-side bugs. State is unchanged.
    #[error("block {} already in buffer (duplicate delivery)", hex::encode(block_id.0))]
    DuplicateBlock { block_id: BlockId },
}

/// The reorg engine. Maintains the canonical chain in `canonical`
/// (front = oldest, back = tip) plus a `HashSet` of ids for O(1)
/// ancestor lookup. `last_irreversible_height` is the high-water
/// mark past which reorgs are refused.
#[derive(Debug)]
pub struct ReorgEngine {
    canonical: VecDeque<BufferedBlock>,
    canonical_ids: HashSet<BlockId>,
    config: ReorgConfig,
    /// Highest height ever emitted as Irreversible. Used to reject
    /// would-be reorgs that try to cross the finality boundary; also
    /// used to guard `Irreversible` emission from re-firing on the
    /// same block.
    last_irreversible_height: Option<BlockHeight>,
    /// Latest chain-reported solidified-head height, sourced from
    /// `WalletSolidity.GetNowBlock` via the service. The engine never
    /// recomputes finality; this is the only finality oracle.
    /// `None` until the first successful fetch.
    solidified_head: Option<BlockHeight>,
    /// Pending reorg observations awaiting resolution by the chain's
    /// solidified head. When `solidified_head` advances past an entry's
    /// `observed_at_height`, the engine emits `Output::ForkResolved`
    /// and drops the entry. Bounded by buffer_window + a small slack
    /// — observations older than the buffer can no longer be resolved
    /// against canonical state and are dropped silently.
    fork_history: VecDeque<ForkObservation>,
    /// Counter for fork_id on `BufferedBlock`. Bumped each time we
    /// replace a block at the same height via reorg, so two cursors
    /// at the same (height, block_id) but across different reorg
    /// histories don't collide.
    next_fork_id: u32,
}

impl ReorgEngine {
    pub fn new(config: ReorgConfig) -> Self {
        assert!(
            config.buffer_window > config.finality_depth,
            "buffer_window ({}) must exceed finality_depth ({}) so finality emission has the block",
            config.buffer_window,
            config.finality_depth,
        );
        Self {
            canonical: VecDeque::with_capacity(config.buffer_window + 1),
            canonical_ids: HashSet::with_capacity(config.buffer_window + 1),
            config,
            last_irreversible_height: None,
            solidified_head: None,
            fork_history: VecDeque::new(),
            next_fork_id: 0,
        }
    }

    /// Current canonical tip, or None on a fresh engine.
    pub fn tip(&self) -> Option<&BufferedBlock> {
        self.canonical.back()
    }

    /// Update the chain-reported solidified head. Called by the
    /// service after every successful `WalletSolidity.GetNowBlock`
    /// fetch. The engine uses this to derive finality on emission and
    /// to gate `Irreversible` / `ForkResolved` outputs.
    ///
    /// Returns the outputs (if any) triggered by the head advancing —
    /// freshly-finalized buffered blocks emit as `Irreversible`, and
    /// pending fork observations whose `observed_at_height` is now
    /// at-or-below the new head emit as `ForkResolved`. Caller forwards
    /// these on the same channel as `accept`'s outputs.
    ///
    /// Idempotent for a head that hasn't advanced. Treats a regression
    /// (`new_head < self.solidified_head`) as a chain-level oddity and
    /// ignores it (does not regress local state).
    pub fn set_solidified_head(&mut self, new_head: BlockHeight) -> Vec<Output> {
        match self.solidified_head {
            Some(prev) if new_head <= prev => return Vec::new(),
            _ => self.solidified_head = Some(new_head),
        }
        let mut out = Vec::new();
        self.maybe_emit_irreversible(&mut out);
        self.maybe_resolve_forks(&mut out);
        out
    }

    /// Read the current chain-reported solidified head, if any. Test
    /// + diagnostic surface.
    pub fn solidified_head(&self) -> Option<BlockHeight> {
        self.solidified_head
    }

    /// The number of canonical blocks currently buffered. Bounded by
    /// `config.buffer_window`.
    pub fn len(&self) -> usize {
        self.canonical.len()
    }

    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }

    /// The ingest entry point. Returns the ordered events to forward
    /// to downstream consumers, or a typed error if the block can't
    /// be incorporated. On error, internal state is unchanged.
    ///
    /// `tx_infos` is the side-channel `TransactionInfo` payload for the
    /// txs in `decoded`, joined by the service before the engine sees
    /// the block. Pass an empty `Vec` when the service is running
    /// without tx-info fetching.
    pub fn accept(
        &mut self,
        decoded: DecodedBlock,
        tx_infos: Vec<DecodedTxInfo>,
    ) -> Result<Vec<Output>, ReorgError> {
        let height = decoded.header.height;
        let block_id = decoded.header.block_id;
        let parent_id = decoded.header.parent_id;

        if self.canonical_ids.contains(&block_id) {
            return Err(ReorgError::DuplicateBlock { block_id });
        }

        // First block: trust it as canonical, set the fork.
        if self.canonical.is_empty() {
            return Ok(self.push_canonical_first(decoded, tx_infos));
        }

        let tip = self.canonical.back().expect("non-empty checked above");

        // Linear extension: cheapest path, the common case at chain head.
        if parent_id == tip.block_id {
            return Ok(self.extend_canonical(decoded, tx_infos));
        }

        // Reorg candidate: parent is somewhere in our canonical buffer
        // but not at the tip. Walk to find the ancestor index.
        if self.canonical_ids.contains(&parent_id) {
            return self.handle_reorg(decoded, tx_infos);
        }

        // Otherwise we don't know this block's parent. Either we have
        // a gap or the parent is older than our buffer window.
        Err(ReorgError::ParentNotInBuffer {
            height,
            parent: parent_id,
            tip_height: tip.height,
            window: self.config.buffer_window,
        })
    }

    fn push_canonical_first(
        &mut self,
        decoded: DecodedBlock,
        tx_infos: Vec<DecodedTxInfo>,
    ) -> Vec<Output> {
        let buffered = BufferedBlock {
            height: decoded.header.height,
            block_id: decoded.header.block_id,
            parent_id: decoded.header.parent_id,
            fork_id: self.alloc_fork_id(),
            decoded,
            tx_infos,
        };
        self.canonical_ids.insert(buffered.block_id);
        let cursor = Cursor::new(buffered.height, buffered.block_id);
        let finality = self.derive_finality_for_tip(buffered.height);
        self.canonical.push_back(buffered.clone());
        let new = StreamableBlock {
            step: Step::New,
            block: buffered,
            cursor,
            finality,
        };
        let mut out = vec![Output::New(new)];
        self.maybe_emit_irreversible(&mut out);
        self.maybe_resolve_forks(&mut out);
        self.maybe_prune_buffer();
        out
    }

    fn extend_canonical(
        &mut self,
        decoded: DecodedBlock,
        tx_infos: Vec<DecodedTxInfo>,
    ) -> Vec<Output> {
        let buffered = BufferedBlock {
            height: decoded.header.height,
            block_id: decoded.header.block_id,
            parent_id: decoded.header.parent_id,
            fork_id: self.alloc_fork_id(),
            decoded,
            tx_infos,
        };
        self.canonical_ids.insert(buffered.block_id);
        let cursor = Cursor::new(buffered.height, buffered.block_id);
        let finality = self.derive_finality_for_tip(buffered.height);
        self.canonical.push_back(buffered.clone());
        let mut out = vec![Output::New(StreamableBlock {
            step: Step::New,
            block: buffered,
            cursor,
            finality,
        })];
        self.maybe_emit_irreversible(&mut out);
        self.maybe_resolve_forks(&mut out);
        self.maybe_prune_buffer();
        out
    }

    /// Build the finality envelope for a block being emitted at the
    /// canonical tip — by definition no buffered descendant.
    fn derive_finality_for_tip(&self, height: BlockHeight) -> BlockFinality {
        BlockFinality::for_block(height, self.solidified_head, false)
    }

    fn handle_reorg(
        &mut self,
        decoded: DecodedBlock,
        tx_infos: Vec<DecodedTxInfo>,
    ) -> Result<Vec<Output>, ReorgError> {
        // Find the ancestor block_id in canonical.back-to-front order.
        // ancestor_idx is the index of the block whose id == parent_id.
        let parent_id = decoded.header.parent_id;
        let ancestor_idx = self
            .canonical
            .iter()
            .rposition(|b| b.block_id == parent_id)
            .expect("caller verified canonical_ids contains parent_id");

        // The ancestor itself stays canonical. Everything strictly
        // after it is being orphaned.
        let undo_count = self.canonical.len() - 1 - ancestor_idx;
        if undo_count == 0 {
            // Means parent_id == tip.id, but we already short-circuited
            // that path. Defensive: treat as linear extension.
            return Ok(self.extend_canonical(decoded, tx_infos));
        }

        // Refuse reorg if it would orphan a finalized block.
        if let Some(li_height) = self.last_irreversible_height {
            let earliest_orphan_height = self.canonical[ancestor_idx + 1].height;
            if earliest_orphan_height <= li_height {
                return Err(ReorgError::ReorgBelowFinality {
                    height: decoded.header.height,
                    last_irreversible_height: li_height,
                });
            }
        }

        // Emit UNDO for each orphaned block, tip-first (reverse order
        // of original NEW emission). Build the outputs first so we
        // can mutate canonical after. Also record the orphaned ids so
        // we can attach them to the ForkObserved ledger entry below.
        let mut out = Vec::with_capacity(undo_count + 2);
        let mut orphaned_tips: Vec<BlockId> = Vec::with_capacity(undo_count);
        for orphan_idx in (ancestor_idx + 1..self.canonical.len()).rev() {
            let orphan = &self.canonical[orphan_idx];
            let cursor = Cursor::new(orphan.height, orphan.block_id);
            // Orphans carry `Seen` finality: by construction they're
            // above the solidified head (we refused this reorg
            // otherwise) AND no longer have a canonical descendant.
            let finality = BlockFinality::for_block(orphan.height, self.solidified_head, false);
            out.push(Output::Undo(StreamableBlock {
                step: Step::Undo,
                block: orphan.clone(),
                cursor,
                finality,
            }));
            orphaned_tips.push(orphan.block_id);
        }

        // Drop orphans from canonical state.
        for _ in 0..undo_count {
            let dropped = self.canonical.pop_back().expect("checked non-empty");
            self.canonical_ids.remove(&dropped.block_id);
        }

        // Now extend with the new block. Allocate a fresh fork_id so
        // (height, block_id, fork_id) tuples remain distinct from any
        // earlier block at this height that just got undone.
        let buffered = BufferedBlock {
            height: decoded.header.height,
            block_id: decoded.header.block_id,
            parent_id: decoded.header.parent_id,
            fork_id: self.alloc_fork_id(),
            decoded,
            tx_infos,
        };
        self.canonical_ids.insert(buffered.block_id);
        let cursor = Cursor::new(buffered.height, buffered.block_id);
        let finality = self.derive_finality_for_tip(buffered.height);
        let kept_tip_id = buffered.block_id;
        let observed_at_height = buffered.height;
        self.canonical.push_back(buffered.clone());
        out.push(Output::New(StreamableBlock {
            step: Step::New,
            block: buffered,
            cursor,
            finality,
        }));

        // Record the fork observation. ADR-0021 §3: we don't decide
        // which fork wins, we record that we saw a competing tip, did
        // an optimistic switch, and are now waiting for the chain's
        // solidified head to confirm the resolution. ForkResolved
        // fires when set_solidified_head crosses observed_at_height.
        out.push(Output::ForkObserved {
            observed_at_height,
            kept_tip: kept_tip_id,
            orphaned_tips: orphaned_tips.clone(),
        });
        self.fork_history.push_back(ForkObservation {
            observed_at_height,
            kept_tip: kept_tip_id,
            orphaned_tips,
        });

        self.maybe_emit_irreversible(&mut out);
        self.maybe_resolve_forks(&mut out);
        self.maybe_prune_buffer();
        Ok(out)
    }

    fn maybe_emit_irreversible(&mut self, out: &mut Vec<Output>) {
        // ADR-0021 §1: finality is read from the chain, never computed.
        // We emit `Irreversible` only when the chain-reported solidified
        // head reaches a buffered block. No solidified head ⇒ no
        // finality claim ⇒ no Irreversible.
        let Some(solidified_head) = self.solidified_head else {
            return;
        };

        // Walk the buffer front-to-back, emitting Irreversible for any
        // block that is at-or-below the solidified head and whose height
        // is strictly greater than what we've already fired for. Most
        // ticks fire 0 or 1; this handles the rare burst case where the
        // solidified-head fetch had been failing and just resumed.
        for block in &self.canonical {
            if block.height > solidified_head {
                break;
            }
            if let Some(prev) = self.last_irreversible_height {
                if block.height <= prev {
                    continue;
                }
            }
            self.last_irreversible_height = Some(block.height);
            let cursor = Cursor::new(block.height, block.block_id);
            // Finality is by definition Finalized when we emit
            // Irreversible — schema-enforce per ADR-0015.
            let finality = BlockFinality {
                tier: FinalityTier::Finalized,
                solidified_head: Some(solidified_head),
            };
            out.push(Output::Irreversible(StreamableBlock {
                step: Step::Irreversible,
                block: block.clone(),
                cursor,
                finality,
            }));
        }
    }

    /// Drain pending fork observations whose `observed_at_height` is
    /// at-or-below the chain's solidified head, emitting one
    /// `ForkResolved` ledger entry per observation. The chain has
    /// resolved which tip survives — we just record it; we don't
    /// decide.
    fn maybe_resolve_forks(&mut self, out: &mut Vec<Output>) {
        let Some(solidified_head) = self.solidified_head else {
            return;
        };
        while let Some(front) = self.fork_history.front() {
            if front.observed_at_height > solidified_head {
                break;
            }
            let obs = self.fork_history.pop_front().expect("front existed");
            out.push(Output::ForkResolved {
                observed_at_height: obs.observed_at_height,
                finalized_head: solidified_head,
                finalized_tip: obs.kept_tip,
                orphaned_tips: obs.orphaned_tips,
            });
        }
    }

    fn maybe_prune_buffer(&mut self) {
        while self.canonical.len() > self.config.buffer_window {
            let dropped = self.canonical.pop_front().expect("len > window > 0");
            self.canonical_ids.remove(&dropped.block_id);
        }
    }

    fn alloc_fork_id(&mut self) -> u32 {
        let id = self.next_fork_id;
        self.next_fork_id = self.next_fork_id.wrapping_add(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_codec::DecodedHeader;
    use lightcycle_types::Address;

    /// Build a synthetic DecodedBlock at the given height with the
    /// given parent_id. We don't build a real protobuf round-trip
    /// here — the engine doesn't care about anything beyond the
    /// header fields it reads.
    fn synth_block(height: u64, block_id: [u8; 32], parent_id: [u8; 32]) -> DecodedBlock {
        DecodedBlock {
            header: DecodedHeader {
                height,
                block_id: BlockId(block_id),
                parent_id: BlockId(parent_id),
                raw_data_hash: [0u8; 32],
                tx_trie_root: [0u8; 32],
                timestamp_ms: 1_777_854_558_000,
                witness: Address([0x41; 21]),
                witness_signature: vec![0u8; 65],
                version: 34,
            },
            transactions: vec![],
        }
    }

    /// Distinct block_id by height + tag, useful for forks.
    fn id(height: u64, tag: u8) -> [u8; 32] {
        let mut a = [tag; 32];
        a[..8].copy_from_slice(&height.to_be_bytes());
        a
    }

    fn small_engine() -> ReorgEngine {
        ReorgEngine::new(ReorgConfig {
            buffer_window: 5,
            finality_depth: 3,
        })
    }

    /// Test convenience: most engine tests don't exercise the
    /// `tx_infos` join, so this wraps the real `accept` with an empty
    /// vec. Tests that DO need to verify tx_info preservation through
    /// the engine call `engine.accept(decoded, tx_infos)` directly.
    fn accept(engine: &mut ReorgEngine, b: DecodedBlock) -> Result<Vec<Output>, ReorgError> {
        engine.accept(b, Vec::new())
    }

    #[test]
    fn linear_extension_emits_new_per_block() {
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..103 {
            let b = synth_block(h, id(h, 0xa), prev);
            let outs = accept(&mut e, b).expect("accept");
            // First two heights: just NEW. Third (which crosses no
            // boundary yet — finality_depth=3 means we need 4 total
            // before emission) also just NEW.
            assert!(matches!(outs[..], [Output::New(_)]));
            prev = id(h, 0xa);
        }
        assert_eq!(e.len(), 3);
    }

    #[test]
    fn finality_emits_only_when_chain_solidifies() {
        // ADR-0021 invariant: Irreversible fires only when the
        // chain-reported solidified head reaches a buffered block.
        // Without a head, no Irreversible at all.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        let mut all_outputs = Vec::new();
        for h in 100..104 {
            let b = synth_block(h, id(h, 0xa), prev);
            all_outputs.extend(accept(&mut e, b).expect("accept"));
            prev = id(h, 0xa);
        }
        let irrev: Vec<_> = all_outputs
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert!(
            irrev.is_empty(),
            "no Irreversible should fire without a solidified head, got {irrev:?}"
        );

        // Chain reports head at 101; we should fire for 100 + 101 only.
        let head_outs = e.set_solidified_head(101);
        let irrev: Vec<_> = head_outs
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(irrev, vec![100, 101]);
    }

    #[test]
    fn finality_doesnt_re_emit_on_repeated_head_set() {
        // set_solidified_head with the same head twice should be a no-op.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        // Stay within buffer_window=5 so all blocks remain available
        // for finality emission.
        for h in 100..103 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).expect("accept");
            prev = id(h, 0xa);
        }
        let first = e.set_solidified_head(102);
        let second = e.set_solidified_head(102);
        let first_irrev: Vec<_> = first
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(first_irrev, vec![100, 101, 102]);
        assert!(second.is_empty(), "repeated head set should be a no-op");
    }

    #[test]
    fn finality_advancing_head_emits_in_order() {
        // Bursting: head was unavailable for several blocks then jumps.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..105 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).expect("accept");
            prev = id(h, 0xa);
        }
        // First, head jumps to 102 — fires for 100, 101, 102.
        let outs = e.set_solidified_head(102);
        let h: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(h, vec![100, 101, 102]);
        // Then advances to 104 — fires for 103, 104 only.
        let outs = e.set_solidified_head(104);
        let h: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(h, vec![103, 104]);
    }

    #[test]
    fn one_block_reorg_emits_undo_then_new() {
        let mut e = small_engine();
        // Build canonical up to height 102 on fork 'a'.
        let mut prev = id(99, 0);
        for h in 100..103 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        // Now arrive a sibling 102b, parent = 101a. Should UNDO 102a +
        // NEW 102b + ForkObserved (the audit ledger entry).
        let outs =
            accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa))).expect("accept reorg");
        let undo_new: Vec<_> = outs
            .iter()
            .filter(|o| matches!(o, Output::Undo(_) | Output::New(_)))
            .collect();
        assert_eq!(undo_new.len(), 2);
        match (&undo_new[0], &undo_new[1]) {
            (Output::Undo(u), Output::New(n)) => {
                assert_eq!(u.block.height, 102);
                assert_eq!(u.block.block_id.0, id(102, 0xa));
                assert_eq!(n.block.height, 102);
                assert_eq!(n.block.block_id.0, id(102, 0xb));
            }
            other => panic!("unexpected outputs: {other:?}"),
        }
        assert_eq!(e.tip().unwrap().block_id.0, id(102, 0xb));
    }

    #[test]
    fn two_block_reorg_emits_undos_in_tip_first_order() {
        let mut e = small_engine();
        // Canonical to 103a.
        let mut prev = id(99, 0);
        for h in 100..104 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        // Branch from 101a: arrive a single new block at height 102b.
        // Should UNDO 103a then UNDO 102a then NEW 102b.
        let outs = accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa))).expect("accept");
        let kinds: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Output::Undo(s) => Some(("undo", s.block.height)),
                Output::New(s) => Some(("new", s.block.height)),
                Output::Irreversible(s) => Some(("irrev", s.block.height)),
                // ForkObserved / ForkResolved are ledger entries — not
                // ordered with Undo/New, so this assertion ignores them.
                Output::ForkObserved { .. } | Output::ForkResolved { .. } => None,
            })
            .collect();
        assert_eq!(kinds, vec![("undo", 103), ("undo", 102), ("new", 102)],);
    }

    #[test]
    fn reorg_below_finality_is_rejected() {
        // Now finality is chain-driven: we explicitly mark heights
        // as finalized via set_solidified_head, then refuse a reorg
        // that would orphan a finalized block.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..107 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        // Chain solidifies through height 103. last_irreversible_height
        // becomes 103, so any reorg whose orphan tail reaches 103 or
        // below is refused.
        let _ = e.set_solidified_head(103);
        let err = accept(&mut e, synth_block(103, id(103, 0xb), id(102, 0xa))).unwrap_err();
        assert!(
            matches!(err, ReorgError::ReorgBelowFinality { .. }),
            "expected ReorgBelowFinality, got {err:?}"
        );
        // State unchanged — still on fork 'a'.
        assert_eq!(e.tip().unwrap().block_id.0, id(106, 0xa));
    }

    #[test]
    fn parent_not_in_buffer_returns_typed_error() {
        let mut e = small_engine();
        accept(&mut e, synth_block(100, id(100, 0xa), id(99, 0))).unwrap();
        // Skip 101; deliver 102 with parent 101 (which we never saw).
        let err = accept(&mut e, synth_block(102, id(102, 0xa), id(101, 0xa))).unwrap_err();
        match err {
            ReorgError::ParentNotInBuffer {
                height: 102,
                parent: BlockId(p),
                ..
            } => assert_eq!(p, id(101, 0xa)),
            other => panic!("expected ParentNotInBuffer, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_block_returns_error_no_state_change() {
        let mut e = small_engine();
        let b = synth_block(100, id(100, 0xa), id(99, 0));
        accept(&mut e, b.clone()).unwrap();
        let err = accept(&mut e, b).unwrap_err();
        assert!(matches!(err, ReorgError::DuplicateBlock { .. }));
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn buffer_window_bounded() {
        // window = 5; deliver 8 blocks, oldest 3 should drop out.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..108 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        assert_eq!(e.len(), 5);
        // Earliest still in buffer is height 103, latest is 107.
        let heights: Vec<_> = e.canonical.iter().map(|b| b.height).collect();
        assert_eq!(heights, vec![103, 104, 105, 106, 107]);
    }

    #[test]
    fn cursor_carries_height_and_block_id() {
        let mut e = small_engine();
        let outs = accept(&mut e, synth_block(100, id(100, 0xa), id(99, 0))).unwrap();
        match &outs[0] {
            Output::New(s) => {
                assert_eq!(s.cursor.height, 100);
                assert_eq!(s.cursor.block_id.0, id(100, 0xa));
            }
            _ => panic!("expected New"),
        }
    }

    #[test]
    fn fork_ids_are_unique_across_a_reorg() {
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..103 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        let outs = accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa))).unwrap();
        // Find the New(102b) and confirm its fork_id differs from
        // whatever 102a got. We don't assert specific values (the
        // counter is implementation detail) — just that they're not
        // equal, which is what cursor disambiguation needs.
        let new_b_fork_id = outs
            .iter()
            .find_map(|o| match o {
                Output::New(s) if s.block.height == 102 => Some(s.block.fork_id),
                _ => None,
            })
            .unwrap();
        let undo_a_fork_id = outs
            .iter()
            .find_map(|o| match o {
                Output::Undo(s) if s.block.height == 102 => Some(s.block.fork_id),
                _ => None,
            })
            .unwrap();
        assert_ne!(new_b_fork_id, undo_a_fork_id);
    }

    #[test]
    fn new_block_carries_finality_envelope() {
        // ADR-0021 §1: every emitted record carries a tagged finality
        // tier. Before any solidified head is set, tier is Seen and
        // solidified_head is None.
        let mut e = small_engine();
        let outs = accept(&mut e, synth_block(100, id(100, 0xa), id(99, 0))).expect("accept");
        match &outs[0] {
            Output::New(s) => {
                assert_eq!(s.finality.tier, FinalityTier::Seen);
                assert_eq!(s.finality.solidified_head, None);
            }
            _ => panic!("expected New"),
        }
    }

    #[test]
    fn finality_envelope_uses_chain_head_when_set() {
        let mut e = small_engine();
        let _ = e.set_solidified_head(105);
        let outs = accept(&mut e, synth_block(100, id(100, 0xa), id(99, 0))).expect("accept");
        // 100 ≤ 105 ⇒ Finalized. Wait — actually no: the engine emits
        // a NEW for 100, then maybe_emit_irreversible fires for 100
        // because 100 ≤ 105. The NEW carries Finalized tier? No —
        // derive_finality_for_tip is computed BEFORE Irreversible is
        // emitted, but it still uses the head, so 100 ≤ 105 →
        // Finalized. Both NEW(100) and Irreversible(100) should carry
        // Finalized tier with head=105.
        match &outs[0] {
            Output::New(s) => {
                assert_eq!(s.finality.tier, FinalityTier::Finalized);
                assert_eq!(s.finality.solidified_head, Some(105));
            }
            _ => panic!("expected New"),
        }
        let irrev = outs
            .iter()
            .find_map(|o| match o {
                Output::Irreversible(s) => Some(s),
                _ => None,
            })
            .expect("Irreversible should fire");
        assert_eq!(irrev.finality.tier, FinalityTier::Finalized);
        assert_eq!(irrev.finality.solidified_head, Some(105));
    }

    #[test]
    fn reorg_emits_fork_observed_ledger_entry() {
        // ADR-0021 §3: on reorg, the engine records a ForkObserved
        // ledger entry naming the kept tip and the orphans. The
        // optimistic switch (Undo→New) still happens, but the entry
        // makes the observation visible to consumers.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..103 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        let outs =
            accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa))).expect("accept reorg");
        let observed = outs
            .iter()
            .find_map(|o| match o {
                Output::ForkObserved {
                    observed_at_height,
                    kept_tip,
                    orphaned_tips,
                } => Some((*observed_at_height, *kept_tip, orphaned_tips.clone())),
                _ => None,
            })
            .expect("ForkObserved should be emitted");
        assert_eq!(observed.0, 102);
        assert_eq!(observed.1 .0, id(102, 0xb));
        assert_eq!(observed.2.len(), 1);
        assert_eq!(observed.2[0].0, id(102, 0xa));
    }

    #[test]
    fn fork_resolves_when_chain_finalizes_past_observation() {
        // ForkObserved at height 102; chain solidifies through 102;
        // engine emits ForkResolved naming the kept tip.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..103 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa))).expect("reorg");
        // No solidified head yet → no ForkResolved.
        // Solidified head jumps to 105 → ForkResolved fires.
        let outs = e.set_solidified_head(105);
        let resolved = outs
            .iter()
            .find_map(|o| match o {
                Output::ForkResolved {
                    observed_at_height,
                    finalized_head,
                    finalized_tip,
                    orphaned_tips,
                } => Some((
                    *observed_at_height,
                    *finalized_head,
                    *finalized_tip,
                    orphaned_tips.clone(),
                )),
                _ => None,
            })
            .expect("ForkResolved should fire");
        assert_eq!(resolved.0, 102);
        assert_eq!(resolved.1, 105);
        assert_eq!(resolved.2 .0, id(102, 0xb));
        assert_eq!(resolved.3[0].0, id(102, 0xa));
    }

    #[test]
    fn solidified_head_regression_is_ignored() {
        // ADR-0021 failure-mode #4: chain reports a regressing head.
        // Engine ignores it (no state change, no emissions) rather
        // than corrupting last_irreversible_height.
        let mut e = small_engine();
        let _ = e.set_solidified_head(100);
        let outs = e.set_solidified_head(50);
        assert!(outs.is_empty(), "regression should produce no output");
        assert_eq!(e.solidified_head(), Some(100));
    }

    #[test]
    fn tx_infos_are_preserved_into_buffered_block() {
        // Engine doesn't inspect tx_infos but must round-trip them
        // verbatim onto the BufferedBlock that ships in the Output —
        // otherwise the firehose encoder has nothing to populate
        // Transaction.info from. Use a non-empty marker to detect
        // truncation/replacement.
        use lightcycle_codec::{DecodedTxInfo, ResourceCost};
        use lightcycle_types::{Address, TxHash};

        let mut e = small_engine();
        let block = synth_block(100, id(100, 0xa), id(99, 0));
        let infos = vec![DecodedTxInfo {
            tx_hash: TxHash([0x42; 32]),
            block_height: 100,
            block_timestamp_ms: 1_777_854_558_000,
            fee_sun: 1_000_000,
            success: true,
            contract_address: Some(Address([0x41; 21])),
            resource: ResourceCost::default(),
            logs: vec![],
            internal_transactions: vec![],
        }];
        let outs = e.accept(block, infos.clone()).unwrap();
        match &outs[0] {
            Output::New(s) => {
                assert_eq!(s.block.tx_infos.len(), 1);
                assert_eq!(s.block.tx_infos[0].tx_hash, infos[0].tx_hash);
                assert_eq!(s.block.tx_infos[0].fee_sun, 1_000_000);
            }
            _ => panic!("expected New"),
        }
    }
}
