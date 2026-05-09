//! Relayer orchestrator.
//!
//! Owns the canonical head pointer and the live block buffer. Drives the
//! source → codec → relayer → firehose pipeline. Handles reorg detection,
//! finality tagging, and cursor management.
//!
//! v0.1 surface (this revision):
//!
//! - [`ReorgEngine`] — pure synchronous state machine. `accept(decoded)`
//!   returns the ordered list of [`Output`] events that should be emitted
//!   to downstream consumers: `New` for canonical extension, `Undo`+`New`
//!   on reorg, `Irreversible` when a buffered block crosses finality.
//!   No I/O, no async; the higher-level service that drives the pipeline
//!   from a source channel is a separate concern.
//!
//! - [`StreamableBlock`] — the unit emitted to consumers. Carries the
//!   step semantics, the decoded payload, and the resumption cursor.
//!
//! - [`Cursor`] format: `[height:8 BE][block_id:32]`. Opaque to consumers
//!   but recoverable: the firehose layer uses it to resume a stream
//!   across restarts.
//!
//! Out of scope here, intentionally:
//!
//! - **Multi-block sibling-graft reorgs.** v0.1 handles 1+ block reorgs
//!   where the new block's `parent_id` is somewhere in the current
//!   canonical chain. Reorgs that arrive via a sibling chain (peer
//!   delivers `100B`, then `101B` extending `100B`, then we have to
//!   compute the longer-chain switch) require sibling buffering. TRON's
//!   actual reorg distribution is tail-heavy at depth 1; the deeper
//!   path lands when we have a real-mainnet sample showing it matters.
//! - **gRPC streaming surface.** That's [`lightcycle-firehose`]'s job.
//!   The engine produces [`Output`]s; the firehose multiplexes them.

#![allow(dead_code)]

mod cursor;
mod engine;
mod service;

pub use cursor::Cursor;
pub use engine::{
    BufferedBlock, Output, ReorgConfig, ReorgEngine, ReorgError, StreamableBlock,
};
pub use service::{
    describe_metrics, static_sr_set, BlockFetcher, FetchedBlock, GrpcBlockFetcher, RelayerService,
    ServiceError, VerifyPolicy,
};
