//! Codec error types. Kept in this crate (not in `lightcycle-types`)
//! so the types crate stays free of `prost` and protobuf concepts.

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
    BadHash {
        field: &'static str,
        got: usize,
    },

    /// TRON addresses are 21 bytes (1-byte network prefix + 20-byte
    /// hash). Wrong-length witness/sender/receiver address.
    #[error("expected 21-byte address ({field}), got {got} bytes")]
    BadAddress {
        field: &'static str,
        got: usize,
    },

    /// `Transaction.contract` had a `r#type` value not in our vendored
    /// enum. Future TRON forks may add new contract types ahead of our
    /// vendor pin; we surface the raw integer so the relayer can decide
    /// whether to skip or fail.
    #[error("unknown contract type tag {0}")]
    UnknownContractType(i32),
}

/// Crate-local convenience alias; all the codec entry points return this.
pub type Result<T> = std::result::Result<T, CodecError>;
