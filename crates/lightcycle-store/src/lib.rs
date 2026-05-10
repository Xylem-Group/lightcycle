//! Block cache + cursor store for lightcycle.
//!
//! ## Consistency posture (ADR-0021)
//!
//! `lightcycle-store` is the persistence layer; future work wires it
//! across replicas (block cache, cursor store, SR-set checkpoints).
//! This crate is constitutionally bound by ADR-0021 ("Consistency
//! horizons and the distributed-verification floor"):
//!
//! - **Chain finality is the only legal cross-replica consistency
//!   source.** When two replicas of `lightcycle-store` disagree about
//!   the head, the resolution is "wait for chain finality and use the
//!   chain's answer" — see [`ConsistencySource`]. We do NOT build a
//!   custom Raft / Paxos / quorum protocol; the chain's SR consensus
//!   already solves the verification problem and the Das Sarma
//!   round-complexity floor (`~/h/sarma2012-implications.md` §6) makes
//!   any locally-engineered alternative provably worse.
//! - **`as_of(now)` reads against in-memory state require an explicit
//!   staleness tolerance** (ADR-0021 §8). Reads against finalized
//!   storage do not — finalized records are by construction past the
//!   chain's finality window.
//! - **`SEEN ≠ EXISTS` for any cross-replica purpose.** A block that
//!   one replica has SEEN is a candidate for finalization, not a
//!   committed fact. Only `FinalityTier::Finalized` records are safe
//!   to use as a cross-replica truth.
//!
//! Failure modes (ADR-0009 style enumeration):
//!
//! 1. *Chain solidified-head fetch fails for >5 min sustained.* The
//!    SLO histogram (`lightcycle_store_block_seen_to_finalized_seconds`)
//!    breaches; alert fires per [`describe_metrics`]. The relayer
//!    keeps streaming with `tier=Seen|Confirmed`; no `Finalized`
//!    transitions happen until the chain RPC recovers. Replicas in
//!    this regime MUST NOT make cross-replica agreement decisions.
//! 2. *Two replicas observe different `Seen` tips.* By construction
//!    expected behavior — heads can diverge on the order of the
//!    chain's reorg window. Resolution is automatic: when finalized
//!    transitions arrive (chain solidifies), both replicas converge.
//!    No engineering intervention required.
//! 3. *Replica observes a finalized transition from the relayer that
//!    a peer replica disagrees with.* This is a chain-level oddity
//!    (the chain reported different solidified heights to the two
//!    relayers). Surfaces as a divergence in
//!    `lightcycle_store_finalized_height` gauge; investigate the
//!    peer's java-tron, do NOT attempt to reconcile in software.
//!
//! ## Components (incremental build-out, see SCAFFOLD.md priority 5)
//!
//! 1. **[`ConsistencyHorizonObserver`]** — measures the
//!    `seen → finalized` latency that ADR-0021 §"consistency-horizon
//!    SLO" mandates for lightcycle. Currently the only landed surface;
//!    the relayer feeds it observations.
//!
//! 2. **Block cache** — bounded in-memory cache of recent blocks,
//!    indexed by both height and id, generic over the stored value
//!    so this crate doesn't pull in `lightcycle-relayer`. See
//!    [`BlockCache`] / [`SharedBlockCache`]. Wired downstream by the
//!    relayer (writes on every emission) and by the firehose Fetch
//!    handler (reads). Spill to `redb` for blocks that age past the
//!    bounded window is a follow-up.
//!
//! 3. **Cursor store** — per-consumer `{height, blockId, forkId}`
//!    checkpoints persisted in a redb-backed file so a consumer
//!    reconnecting after a process restart resumes deterministically.
//!    See [`CursorStore`]. Available; the firehose layer wires it
//!    in when the operator passes a path. Per ADR-0021 the store is
//!    explicitly NOT a cross-replica consistency primitive — each
//!    replica tracks the consumers attached to it.
//!
//! 4. **SR set checkpoints** — trusted starting point + maintenance-period
//!    diffs of the active witness set. Cold restarts re-derive from the
//!    nearest checkpoint instead of replaying from genesis. Not yet wired.

mod cache;
mod consistency;
mod cursor_store;

pub use cache::{describe_cache_metrics, new_shared, BlockCache, SharedBlockCache};
pub use consistency::{
    describe_metrics, ConsistencyHorizonObserver, ConsistencySource, FinalityFromChain,
    BLOCK_SEEN_TO_FINALIZED_SECONDS_METRIC,
};
pub use cursor_store::{describe_cursor_store_metrics, CursorStore, CursorStoreError};
