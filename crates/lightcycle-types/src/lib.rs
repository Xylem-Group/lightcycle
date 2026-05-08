//! Core domain types for lightcycle.
//!
//! This crate is intentionally thin: domain primitives only, no I/O, no protobuf,
//! no async. Everything downstream depends on these types.

use serde::{Deserialize, Serialize};

/// A TRON block height (a.k.a. "block number").
pub type BlockHeight = u64;

/// A 32-byte block identifier (TRON's `blockId` is height-prefixed but we store the full 32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockId(pub [u8; 32]);

/// A 32-byte transaction hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxHash(pub [u8; 32]);

/// A 21-byte TRON address (1 byte network prefix + 20 byte hash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address(pub [u8; 21]);

/// Streaming step semantics, matching Firehose's `ForkStep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// New canonical block.
    New,
    /// Block was orphaned by a reorg; consumers should undo its effects.
    Undo,
    /// Block has crossed solidification (~19 confirmations on TRON).
    Irreversible,
}

/// Opaque cursor for stream resumption. Encodes `(height, blockId, forkId)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor(pub Vec<u8>);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid block id: {0}")]
    InvalidBlockId(String),
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}

pub type Result<T> = std::result::Result<T, Error>;
