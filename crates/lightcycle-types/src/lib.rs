//! Core domain types for lightcycle.
//!
//! This crate is intentionally thin: domain primitives only, no I/O, no protobuf,
//! no async. Everything downstream depends on these types.

use std::collections::HashSet;

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

impl Address {
    /// Encode as base58check, the form TRON addresses take in every
    /// human-facing UI ("T..." string, 34 chars on mainnet). Uses the
    /// Bitcoin-style 4-byte double-sha256 checksum that TRON inherits.
    pub fn to_base58check(&self) -> String {
        bs58::encode(self.0).with_check().into_string()
    }

    /// Parse a base58check string back into an Address. Validates the
    /// checksum and verifies the decoded length is exactly 21 bytes.
    /// Does NOT enforce the `0x41` mainnet prefix — testnet uses
    /// `0xa0`, and consumers may want to check explicitly.
    pub fn from_base58check(s: &str) -> Result<Self> {
        let bytes = bs58::decode(s)
            .with_check(None)
            .into_vec()
            .map_err(|e| Error::InvalidAddress(format!("base58check: {e}")))?;
        if bytes.len() != 21 {
            return Err(Error::InvalidAddress(format!(
                "expected 21 bytes after base58check decode, got {}",
                bytes.len()
            )));
        }
        let mut a = [0u8; 21];
        a.copy_from_slice(&bytes);
        Ok(Self(a))
    }
}

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

/// Active super-representative set — the addresses currently authorized
/// to produce blocks. TRON's canonical SR set has 27 active members at
/// any given epoch, but `wallet/listwitnesses` returns the full
/// configured witness list (including standby SRPs); the source layer
/// filters to the active subset before constructing this.
///
/// The only operation the codec needs is O(1) membership check, so
/// `HashSet` over `Address` is the right shape. Set transitions across
/// SR rotations are tracked at the source layer (a new `SrSet` is built
/// per epoch); the codec is stateless w.r.t. epoch boundaries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SrSet {
    members: HashSet<Address>,
}

impl SrSet {
    /// Build an SR set from any iterable of addresses. Duplicates are
    /// silently deduped — feeding `listwitnesses` output directly is fine.
    pub fn new<I: IntoIterator<Item = Address>>(members: I) -> Self {
        Self {
            members: members.into_iter().collect(),
        }
    }

    pub fn contains(&self, address: &Address) -> bool {
        self.members.contains(address)
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Address> {
        self.members.iter()
    }
}

impl FromIterator<Address> for SrSet {
    fn from_iter<I: IntoIterator<Item = Address>>(iter: I) -> Self {
        Self::new(iter)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid block id: {0}")]
    InvalidBlockId(String),
    #[error("invalid address: {0}")]
    InvalidAddress(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58check_round_trip_through_known_address() {
        // Sun network's USDT contract address — pinned widely-known
        // mainnet address used as a regression check on the TRX→T...
        // mapping.
        let s = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
        let addr = Address::from_base58check(s).expect("decode");
        assert_eq!(addr.to_base58check(), s);
        assert_eq!(addr.0[0], 0x41, "USDT mainnet address should have 0x41 prefix");
    }

    #[test]
    fn base58check_rejects_bad_checksum() {
        // Flip the last char to break the checksum.
        let bad = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6T";
        let err = Address::from_base58check(bad).unwrap_err();
        assert!(matches!(err, Error::InvalidAddress(_)));
    }

    #[test]
    fn base58check_rejects_non_21_byte_payload() {
        // Construct a base58check of a 20-byte payload (Ethereum-style)
        // and confirm we reject it. Skipping the wrapper here — easier
        // to use bs58 directly to assemble.
        let twenty = [0xab; 20];
        let s = bs58::encode(twenty).with_check().into_string();
        let err = Address::from_base58check(&s).unwrap_err();
        assert!(matches!(err, Error::InvalidAddress(_)));
    }
}
