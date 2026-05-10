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
//!   call `decode_contract_payload` (TODO; lands when a consumer
//!   asks).
//! - [`ContractKind`] — friendly enum mirroring java-tron's wire
//!   `ContractType` with the awkward `Contract` suffix dropped and an
//!   `Other(i32)` for forward-compat.
//!
//! - [`verify_witness_signature`] / [`recover_witness_address`] —
//!   secp256k1 ECDSA recovery against the active SR set. Reuses the
//!   `sha256(raw_data)` digest already computed during decode (it's
//!   the `block_id`).
//!
//! - [`decode_transaction_info`] / [`decode_transaction_info_list`] —
//!   parse the side-channel `TransactionInfo` (logs, internal txs,
//!   resource accounting) the source layer fetches via
//!   `getTransactionInfoByBlockNum`. The block proto doesn't carry
//!   logs or sub-call traces; this is how consumers see them.
//!
//! - [`decode_trc20_transfer`] / [`decode_trc20_approval`] — the two
//!   universal TRC-20 events recognized by topic hash. Standalone,
//!   ABI-registry-free helpers; full-ABI decoding is future work.
//!
//! Deferred (separate entry points, separate crates' job to provide
//! inputs):
//!
//! - **Event log decoding (arbitrary contracts).** Needs an ABI
//!   registry. Lives behind a future `decode_event(log, abi)` entry
//!   point. v0.1 ships only the universal TRC-20 helpers above plus
//!   raw [`Log`] passthrough.
//! - **SM2 sigverify.** Investigated 2026-05-09 (java-tron #6588,
//!   PR #6627): the SM2 codepath exists in `SignUtils` but is dormant
//!   on mainnet — `isECKeyCryptoEngine` is hard-true and no mainnet
//!   SR signs with SM2. Core devs have proposed deleting the unused
//!   module. lightcycle implements only secp256k1; a
//!   `WitnessAddressMismatch` here means a real bug or an attacker,
//!   not a benign engine mismatch.

mod block;
mod error;
mod events;
mod sigverify;
mod transaction;
mod tx_info;

pub use block::{decode_block, decode_block_message, DecodedBlock, DecodedHeader};
pub use error::{CodecError, Result};
pub use events::{
    decode_trc20_approval, decode_trc20_transfer, decode_trc721_transfer, Trc20Approval,
    Trc20Transfer, Trc721Transfer, TRC20_APPROVAL_TOPIC0, TRC20_TRANSFER_TOPIC0,
};
pub use sigverify::{recover_witness_address, verify_witness_signature};
pub use transaction::{ContractKind, DecodedContract, DecodedTransaction};
pub use tx_info::{
    decode_transaction_info, decode_transaction_info_list, decode_transaction_info_list_message,
    decode_transaction_info_message, CallValueInfo, DecodedTxInfo, InternalTx, Log, ResourceCost,
};
