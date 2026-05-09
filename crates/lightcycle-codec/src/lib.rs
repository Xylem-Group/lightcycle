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
//! - **SM2 sigverify.** ~25% of TRON SRs sign with SM2 instead of
//!   secp256k1; pulls in a separate crypto stack. Tracked as a
//!   follow-up; v0.1 returns `WitnessAddressMismatch` on those blocks
//!   and the relayer falls back to "trust the peer" for that header.

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
    decode_transaction_info, decode_transaction_info_list, decode_transaction_info_message,
    CallValueInfo, DecodedTxInfo, InternalTx, Log, ResourceCost,
};
