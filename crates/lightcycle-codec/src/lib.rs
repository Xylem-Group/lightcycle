//! Block decode, signature verification, and event decoding.
//!
//! Pure CPU. No I/O, no async. The hot path of the relayer.
//!
//! v0.1 surface (this revision):
//!
//! - [`decode_block`] / [`decode_block_message`] — parse the wire
//!   `Block` into [`DecodedBlock`] (header + transactions). The
//!   contract `Any` payload on each transaction is intentionally NOT
//!   unwrapped here; consumers that want the typed contract message
//!   call [`decode_contract_payload`] (TODO; lands when a consumer
//!   asks).
//! - [`ContractKind`] — friendly enum mirroring java-tron's wire
//!   `ContractType` with the awkward `Contract` suffix dropped and an
//!   `Other(i32)` for forward-compat.
//!
//! Deferred (separate entry points, separate crates' job to provide
//! inputs):
//!
//! - **Signature verification.** Needs an `SrSet`; the source layer
//!   tracks SR-set transitions. Lives behind a future
//!   `verify_witness_signature(decoded, sr_set)`.
//! - **Event log decoding (TRC-20/721).** Needs an ABI registry. Lives
//!   behind a future `decode_event(topics, data, abi)`.
//! - **Internal transaction extraction.** Surfaces only via
//!   `getTransactionInfoById` over RPC, not the block proto, so it's
//!   the source layer's responsibility to fetch + the codec's to
//!   parse the response.
//! - **Resource accounting.** Composes from execution receipts, again
//!   via `getTransactionInfoById`.

mod block;
mod error;
mod transaction;

pub use block::{decode_block, decode_block_message, DecodedBlock, DecodedHeader};
pub use error::{CodecError, Result};
pub use transaction::{ContractKind, DecodedTransaction};
