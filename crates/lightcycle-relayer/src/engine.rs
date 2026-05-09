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
use lightcycle_types::{BlockHeight, BlockId, Step};
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
    /// Block has crossed the finality threshold; consumers can prune
    /// any reorg-undo state for it.
    Irreversible(StreamableBlock),
}

/// The unit emitted to consumers. Pairs the decoded block with the
/// step semantics and the resumption cursor.
#[derive(Debug, Clone)]
pub struct StreamableBlock {
    pub step: Step,
    pub block: BufferedBlock,
    pub cursor: Cursor,
}

/// Engine configuration. Defaults are TRON-tuned: 30-block buffer
/// covers any plausible reorg depth, 19-block finality matches
/// TRON's solidified-head rule (~57s at 3s blocks).
#[derive(Debug, Clone, Copy)]
pub struct ReorgConfig {
    /// How many canonical blocks to keep in memory. Must be > finality_depth
    /// so finality emission has the buffered block on hand.
    pub buffer_window: usize,
    /// Number of confirmations a block needs before it's emitted as
    /// `Irreversible`. TRON solidifies at ~19/27 SR confirmations.
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
            next_fork_id: 0,
        }
    }

    /// Current canonical tip, or None on a fresh engine.
    pub fn tip(&self) -> Option<&BufferedBlock> {
        self.canonical.back()
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
        self.canonical.push_back(buffered.clone());
        let new = StreamableBlock {
            step: Step::New,
            block: buffered,
            cursor,
        };
        let mut out = vec![Output::New(new)];
        self.maybe_emit_irreversible(&mut out);
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
        self.canonical.push_back(buffered.clone());
        let mut out = vec![Output::New(StreamableBlock {
            step: Step::New,
            block: buffered,
            cursor,
        })];
        self.maybe_emit_irreversible(&mut out);
        self.maybe_prune_buffer();
        out
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
        // can mutate canonical after.
        let mut out = Vec::with_capacity(undo_count + 1);
        for orphan_idx in (ancestor_idx + 1..self.canonical.len()).rev() {
            let orphan = &self.canonical[orphan_idx];
            let cursor = Cursor::new(orphan.height, orphan.block_id);
            out.push(Output::Undo(StreamableBlock {
                step: Step::Undo,
                block: orphan.clone(),
                cursor,
            }));
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
        self.canonical.push_back(buffered.clone());
        out.push(Output::New(StreamableBlock {
            step: Step::New,
            block: buffered,
            cursor,
        }));

        self.maybe_emit_irreversible(&mut out);
        self.maybe_prune_buffer();
        Ok(out)
    }

    fn maybe_emit_irreversible(&mut self, out: &mut Vec<Output>) {
        // The block at canonical[len - 1 - finality_depth] has crossed.
        // If it's already been emitted as Irreversible, skip.
        let len = self.canonical.len();
        if len <= self.config.finality_depth {
            return;
        }
        let crossed_idx = len - 1 - self.config.finality_depth;
        let crossed = &self.canonical[crossed_idx];

        // Don't re-emit. last_irreversible_height tracks the highest
        // we've fired for; only fire when the crossed block's height
        // is strictly higher.
        if let Some(prev) = self.last_irreversible_height {
            if crossed.height <= prev {
                return;
            }
        }
        self.last_irreversible_height = Some(crossed.height);

        let cursor = Cursor::new(crossed.height, crossed.block_id);
        out.push(Output::Irreversible(StreamableBlock {
            step: Step::Irreversible,
            block: crossed.clone(),
            cursor,
        }));
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
    fn finality_emits_at_correct_depth() {
        // finality_depth = 3 means: after we've buffered 4 blocks,
        // the oldest (the one that's now 3-confirmations-deep) should
        // be emitted as Irreversible.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        let mut all_outputs = Vec::new();
        for h in 100..104 {
            let b = synth_block(h, id(h, 0xa), prev);
            all_outputs.extend(accept(&mut e, b).expect("accept"));
            prev = id(h, 0xa);
        }
        // Expected: NEW(100), NEW(101), NEW(102), NEW(103) + IRREVERSIBLE(100).
        let news: Vec<_> = all_outputs
            .iter()
            .filter_map(|o| match o {
                Output::New(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        let irrev: Vec<_> = all_outputs
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(news, vec![100, 101, 102, 103]);
        assert_eq!(irrev, vec![100]);
    }

    #[test]
    fn finality_doesnt_re_emit() {
        // After 5 blocks with depth 3, we've emitted IRREVERSIBLE for
        // 100 and 101. Adding a 6th should fire only for 102, not
        // re-fire for the older ones.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        let mut buf = Vec::new();
        for h in 100..106 {
            let b = synth_block(h, id(h, 0xa), prev);
            buf.extend(accept(&mut e, b).expect("accept"));
            prev = id(h, 0xa);
        }
        let irrev_heights: Vec<_> = buf
            .iter()
            .filter_map(|o| match o {
                Output::Irreversible(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(irrev_heights, vec![100, 101, 102]);
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
        // Now arrive a sibling 102b, parent = 101a. Should UNDO 102a + NEW 102b.
        let outs = accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa)))
            .expect("accept reorg");
        assert_eq!(outs.len(), 2);
        match (&outs[0], &outs[1]) {
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
        let outs = accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa)))
            .expect("accept");
        let kinds: Vec<_> = outs
            .iter()
            .map(|o| match o {
                Output::Undo(s) => ("undo", s.block.height),
                Output::New(s) => ("new", s.block.height),
                Output::Irreversible(s) => ("irrev", s.block.height),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![("undo", 103), ("undo", 102), ("new", 102)],
        );
    }

    #[test]
    fn reorg_below_finality_is_rejected() {
        // window=5, finality=3. After delivering 100..106 inclusive
        // (7 blocks): the oldest block 100 has been pruned out of
        // canonical (too old), and IRREVERSIBLE has fired up through
        // height 103. Canonical state: [101, 102, 103, 104, 105, 106]
        // wait — that's 6 entries. Let me trace this in the test
        // setup loop: by accept(106) we've called maybe_prune twice
        // so canonical = [102..106] = 5 entries. last_irrev = 103.
        let mut e = small_engine();
        let mut prev = id(99, 0);
        for h in 100..107 {
            accept(&mut e, synth_block(h, id(h, 0xa), prev)).unwrap();
            prev = id(h, 0xa);
        }
        // Now try to fork at 102a — its child 103a is below finality.
        // canonical_ids contains 102a; the reorg path runs but the
        // earliest-orphan check fires.
        let err = accept(&mut e, synth_block(103, id(103, 0xb), id(102, 0xa)))
            .unwrap_err();
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
        let err = accept(&mut e, synth_block(102, id(102, 0xa), id(101, 0xa)))
            .unwrap_err();
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
        let outs = accept(&mut e, synth_block(100, id(100, 0xa), id(99, 0)))
            .unwrap();
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
        let outs = accept(&mut e, synth_block(102, id(102, 0xb), id(101, 0xa)))
            .unwrap();
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
