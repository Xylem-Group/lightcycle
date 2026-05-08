//! Block cache + cursor store for lightcycle.
//!
//! Stub. The real surface lands incrementally; see SCAFFOLD.md priority 5.
//! Three pieces planned (in order of build-out):
//!
//! 1. **Block cache** — last N unsolidified blocks held in memory for
//!    reorg replay, with spill to `redb` when N grows. The reorg engine
//!    in `lightcycle-relayer` walks this on canonical-chain switches.
//!
//! 2. **Cursor store** — per-consumer `{height, blockId, forkId}`
//!    checkpoints, persisted so a consumer reconnect after a process
//!    restart resumes deterministically. Optional in v0.1; useful later
//!    for ops dashboards.
//!
//! 3. **SR set checkpoints** — trusted starting point + maintenance-period
//!    diffs of the active witness set. Cold restarts re-derive from the
//!    nearest checkpoint instead of replaying from genesis.
//!
//! The crate exists in the workspace now so dependent crates
//! (`lightcycle-relayer`, `lightcycle-firehose`) can already import it.
