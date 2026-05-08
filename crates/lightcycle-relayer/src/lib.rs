//! Relayer orchestrator.
//!
//! Owns the canonical head pointer and the live block buffer. Drives the
//! source → codec → relayer → firehose pipeline. Handles reorg detection,
//! finality tagging, and cursor management.

#![allow(dead_code)]

use lightcycle_types::{BlockHeight, Step};

/// A block emitted from the relayer to downstream consumers.
#[derive(Debug, Clone)]
pub struct StreamableBlock {
    pub height: BlockHeight,
    pub step: Step,
    // TODO: payload + cursor + decoded body
}

/// The reorg engine: maintains last N unsolidified blocks and emits UNDO/NEW
/// pairs when the canonical chain switches.
#[derive(Debug, Default)]
pub struct ReorgEngine {
    // TODO
}
