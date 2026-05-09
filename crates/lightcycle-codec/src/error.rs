//! Codec error types. Kept in this crate (not in `lightcycle-types`)
//! so the types crate stays free of `prost` and protobuf concepts.

use lightcycle_types::Address;
use thiserror::Error;

/// Anything that can go wrong while decoding a block, transaction, or
/// event log. We surface the upstream prost decode error rather than
/// re-stringifying it; consumers usually want to log the offending bytes
/// or treat malformed-frame as a peer-quality signal.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The protobuf bytes failed to decode against the schema we vendor.
    /// Indicates either a corrupt feed (peer fault) or a schema mismatch
    /// (we're behind upstream and a new field is being sent).
    #[error("protobuf decode failed: {0}")]
    ProtoDecode(#[from] prost::DecodeError),

    /// Block has no `block_header` field set. java-tron always sets this
    /// for valid blocks; missing means the source delivered a stub.
    #[error("block missing block_header")]
    MissingBlockHeader,

    /// `BlockHeader.raw_data` was None. Sibling of `MissingBlockHeader`.
    #[error("block header missing raw_data")]
    MissingHeaderRaw,

    /// `Transaction.raw_data` was None.
    #[error("transaction missing raw_data")]
    MissingTransactionRaw,

    /// A field claimed to be 32 bytes (block hash, tx-trie root, …) had
    /// a different length. Surfaces protocol-shape violations rather
    /// than panicking on a slice copy.
    #[error("expected 32-byte field {field}, got {got} bytes")]
    BadHash { field: &'static str, got: usize },

    /// TRON addresses are 21 bytes (1-byte network prefix + 20-byte
    /// hash). Wrong-length witness/sender/receiver address.
    #[error("expected 21-byte address ({field}), got {got} bytes")]
    BadAddress { field: &'static str, got: usize },

    /// `Transaction.contract` had a `r#type` value not in our vendored
    /// enum. Future TRON forks may add new contract types ahead of our
    /// vendor pin; we surface the raw integer so the relayer can decide
    /// whether to skip or fail.
    #[error("unknown contract type tag {0}")]
    UnknownContractType(i32),

    /// Witness signature (or any 65-byte ECDSA blob) had wrong length.
    /// Surfaces shape violations before we hand bytes to k256.
    #[error("expected 65-byte field {field}, got {got} bytes")]
    BadSignature { field: &'static str, got: usize },

    /// k256 rejected the signature: malformed r/s, point-not-on-curve,
    /// or an invalid recovery id. Surfaces as a single variant because
    /// downstream callers treat them identically — the block is bad.
    #[error("signature recovery failed (malformed signature, bad recovery id, or non-recoverable point)")]
    SignatureRecoveryFailed,

    /// Recovery succeeded but produced an address that doesn't match the
    /// header's claimed `witness`. Indicates a tampered or replayed
    /// signature: the signer was someone other than the declared witness.
    #[error(
        "recovered witness address {} doesn't match header-declared witness {}",
        hex::encode(recovered.0),
        hex::encode(expected.0)
    )]
    WitnessAddressMismatch {
        expected: Address,
        recovered: Address,
    },

    /// The signature is well-formed and self-consistent (recovers to the
    /// header's witness), but that witness is not in the active SR set.
    /// An unauthorized signer; reject the block.
    #[error(
        "witness {} is not in the active SR set ({set_size} members)",
        hex::encode(witness.0)
    )]
    WitnessNotInSrSet { witness: Address, set_size: usize },
}

/// Crate-local convenience alias; all the codec entry points return this.
pub type Result<T> = std::result::Result<T, CodecError>;
