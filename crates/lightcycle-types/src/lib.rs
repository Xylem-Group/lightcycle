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
///
/// `Step` is an *event tag* — what happened — not a block's state.
/// A block's finality state is [`FinalityTier`]; the two are distinct
/// and a block carries both. Example: a `Step::New` emission can carry
/// a `FinalityTier::Seen`, `Confirmed`, or `Finalized` block depending
/// on where the block sits relative to the chain's solidified head at
/// emission time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// New canonical block.
    New,
    /// Block was orphaned by a reorg; consumers should undo its effects.
    Undo,
    /// Block has crossed solidification (~19 confirmations on TRON).
    Irreversible,
}

/// Per-block finality state, derived from the chain's own SR-consensus
/// reporting (NOT recomputed by us). Implements the discipline pinned in
/// ADR-0021 (`alexandria/docs/adr/0021-consistency-horizons-and-the-distributed-verification-floor.md`):
/// finality is read from the chain, never computed; the schema enforces
/// the invariant in the spirit of ADR-0015 (`docs/adr/0015-multi-clock-time-model.md`).
///
/// Tier transitions are monotone in (block_height, solidified_head). A
/// block moves Seen → Confirmed → Finalized; it never moves backward,
/// and a Finalized block is by definition past reorg risk per the chain's
/// own consensus.
///
/// **Failure modes (enumerate, ADR-0009 style):**
///
/// 1. *Solidified-head fetch fails.* Tier derivation degrades to
///    Seen/Confirmed only — Finalized is gated on a fresh head reading.
///    Soft-fail: emission continues with the last-known head until the
///    next successful fetch. Logged + metered.
/// 2. *Block height exceeds last-known solidified head.* Block is at
///    most Confirmed. This is the steady-state head-of-chain regime.
/// 3. *Sibling at the same height under unsolidified head.* Both
///    candidates carry tier ≤ Confirmed. The engine emits a
///    `ForkObserved` ledger entry and waits for the chain's solidified
///    head to advance past the disagreement; whichever block is at
///    `height ≤ solidified_head` *and matches the solidified-head's
///    block_id by ancestry* is the survivor. We do not "decide" — we
///    record the chain's resolution.
/// 4. *Solidified head appears to regress.* Treated as a chain-level
///    bug; surfaced as a typed error rather than silently accepted.
///
/// The wire mapping in `sf.tron.type.v1.Block.finality.tier` is the
/// canonical external representation; this Rust enum is the source of
/// truth for in-process code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinalityTier {
    /// Block has been received and parsed; no chain-finality claim yet.
    /// Equivalent to "exists in the source's view of the canonical
    /// chain" — NOT equivalent to "exists" for any cross-replica or
    /// audit purpose.
    Seen,
    /// Block has been built upon by ≥1 subsequent block in the
    /// engine's canonical buffer. A consensus checkpoint, but still
    /// reorg-risk-bearing until Finalized.
    Confirmed,
    /// Block height ≤ the chain's solidified-head height as last
    /// reported by `WalletSolidity.GetNowBlock`. This is the only tier
    /// safe to use as a consistency source for cross-replica or
    /// cross-region agreement (ADR-0021 §2).
    Finalized,
}

impl FinalityTier {
    /// Derive the tier of a block at `height` given the chain's last
    /// reported solidified head and whether the engine has buffered
    /// any descendant.
    ///
    /// Pure function over inputs — no I/O, no internal mutation, no
    /// hidden state. Callers thread `solidified_head` from a chain
    /// fetch (never from a local clock or a count we computed).
    pub fn derive(
        height: BlockHeight,
        solidified_head: Option<BlockHeight>,
        has_buffered_descendant: bool,
    ) -> Self {
        match solidified_head {
            Some(s) if height <= s => FinalityTier::Finalized,
            _ if has_buffered_descendant => FinalityTier::Confirmed,
            _ => FinalityTier::Seen,
        }
    }

    /// True if a block at this tier is past the chain's finality
    /// threshold. The only legal answer to "is it safe to use this as
    /// a cross-replica consistency source?" (ADR-0021 §2).
    pub fn is_finalized(self) -> bool {
        matches!(self, FinalityTier::Finalized)
    }
}

/// The finality envelope a block carries on every emission. Pairs the
/// derived tier with the chain's last-known solidified-head reference
/// so consumers can both filter by tier and audit *what claim the
/// chain made when we tagged this*. The reference is `None` until the
/// engine has seen at least one successful solidified-head fetch from
/// `WalletSolidity.GetNowBlock` (cold-start window or upstream-RPC
/// failure regime — see [`FinalityTier`] failure modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFinality {
    pub tier: FinalityTier,
    pub solidified_head: Option<BlockHeight>,
}

impl BlockFinality {
    /// Build a finality envelope for a block at `height`, given the
    /// chain's last-known solidified head and whether the engine has
    /// at least one buffered descendant of this block.
    pub fn for_block(
        height: BlockHeight,
        solidified_head: Option<BlockHeight>,
        has_buffered_descendant: bool,
    ) -> Self {
        Self {
            tier: FinalityTier::derive(height, solidified_head, has_buffered_descendant),
            solidified_head,
        }
    }
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
        assert_eq!(
            addr.0[0], 0x41,
            "USDT mainnet address should have 0x41 prefix"
        );
    }

    #[test]
    fn base58check_rejects_bad_checksum() {
        // Flip the last char to break the checksum.
        let bad = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6T";
        let err = Address::from_base58check(bad).unwrap_err();
        assert!(matches!(err, Error::InvalidAddress(_)));
    }

    #[test]
    fn finality_tier_below_solidified_head_is_finalized() {
        // ADR-0021 invariant: tier=Finalized iff the chain says so.
        let t = FinalityTier::derive(100, Some(105), false);
        assert_eq!(t, FinalityTier::Finalized);
        assert!(t.is_finalized());
    }

    #[test]
    fn finality_tier_at_solidified_head_is_finalized() {
        // Boundary: height == solidified_head is finalized (the
        // solidified head is itself solidified).
        assert_eq!(
            FinalityTier::derive(105, Some(105), false),
            FinalityTier::Finalized,
        );
    }

    #[test]
    fn finality_tier_above_head_with_descendant_is_confirmed() {
        // Past the solidified head but with a child built on top —
        // the engine's strongest local claim short of chain finality.
        assert_eq!(
            FinalityTier::derive(110, Some(105), true),
            FinalityTier::Confirmed,
        );
    }

    #[test]
    fn finality_tier_above_head_without_descendant_is_seen() {
        // Tip-of-buffer: no children yet, head not advanced past us.
        assert_eq!(
            FinalityTier::derive(110, Some(105), false),
            FinalityTier::Seen,
        );
    }

    #[test]
    fn finality_tier_without_known_head_never_finalizes() {
        // Failure-mode #1: solidified-head fetch failed; we have no
        // chain claim. Best we can say is Confirmed (with descendant)
        // or Seen. NEVER Finalized.
        assert_eq!(
            FinalityTier::derive(100, None, true),
            FinalityTier::Confirmed,
        );
        assert_eq!(FinalityTier::derive(100, None, false), FinalityTier::Seen,);
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
