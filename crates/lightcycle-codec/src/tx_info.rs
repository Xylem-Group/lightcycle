//! `TransactionInfo` decoder: receipts, logs, internal transactions,
//! and resource accounting.
//!
//! `TransactionInfo` is the side channel java-tron emits for every tx
//! that *actually executed*. The block proto carries the request
//! (signed tx + contract payload); `TransactionInfo` carries the result
//! (success/fail, energy/bandwidth used, emitted event logs, sub-calls
//! the EVM made internally). Consumers that want to reconstruct token
//! flow on TRC-20 / TRC-721 contracts cannot do it from the block
//! alone — they need this side channel.
//!
//! Source layer fetches it via `WalletSolidity::GetTransactionInfoByBlockNum`
//! (preferred — one RPC for the whole block) or `GetTransactionInfoById`
//! (fallback for tail blocks not yet solidified). We expose both a
//! single-info decoder and a list decoder; pick whichever shape the
//! source layer hands us.
//!
//! What we DO here:
//!   - Reshape into domain types ([`DecodedTxInfo`], [`Log`],
//!     [`InternalTx`], [`ResourceCost`]). Strip fields no consumer in
//!     the lightcycle surface uses today (shielded fields, market
//!     order details, exchange amounts) — restore on demand.
//!   - Surface logs raw (address + topics + data). Topic-aware
//!     decoding for the two universal TRC-20 events lives in the
//!     [`super::events`] module; full ABI-driven decoding is future
//!     work behind an injected registry.
//!
//! What we do NOT do:
//!   - Cross-link `DecodedTxInfo` to its parent block. The relayer
//!     owns that join (it has both inputs).
//!   - ABI decoding for arbitrary contract events. Different problem,
//!     different module.

use prost::Message;

use lightcycle_proto::tron::protocol;
use lightcycle_types::{Address, TxHash};

use crate::error::{CodecError, Result};

/// Domain-friendly view of `protocol::TransactionInfo`.
#[derive(Debug, Clone)]
pub struct DecodedTxInfo {
    /// Transaction id (sha256 of the tx's `raw_data`). Matches
    /// `DecodedTransaction::hash` on the block side; this is the join
    /// key consumers use.
    pub tx_hash: TxHash,
    /// Block height this tx was included in.
    pub block_height: u64,
    /// Block timestamp (ms since epoch). Duplicates header info but is
    /// convenient when consumers process tx-info streams independently.
    pub block_timestamp_ms: i64,
    /// Total fee paid in sun (1 TRX = 1e6 sun). Already reflects the
    /// energy/bandwidth cost net of staked resources; consumers don't
    /// need to recompute from `resource`.
    pub fee_sun: i64,
    /// True iff `result == SUCCESS`. False means the EVM reverted or
    /// the contract pre-check failed; the tx is still in the block,
    /// just with no state change.
    pub success: bool,
    /// Contract address for `TriggerSmartContract` / `CreateSmartContract`
    /// txs (21 bytes, 0x41-prefixed). None for non-contract txs.
    pub contract_address: Option<Address>,
    /// Per-tx resource consumption + fee breakdown.
    pub resource: ResourceCost,
    /// EVM event logs emitted during execution. Preserved in the order
    /// they were emitted, which is also the order ABI-decoders need.
    pub logs: Vec<Log>,
    /// Sub-calls the EVM made internally (CALL / DELEGATECALL / etc.
    /// that move TRX or trigger contract logic). Only populated when
    /// the tx hit a smart contract.
    pub internal_transactions: Vec<InternalTx>,
}

/// EVM event log. Topics + data are kept as raw bytes; consumers
/// supply their own ABI registry to decode parameter values.
#[derive(Debug, Clone)]
pub struct Log {
    /// Emitting contract (21 bytes, 0x41-prefixed).
    pub address: Address,
    /// Indexed event parameters. `topics[0]` is the event-signature hash
    /// for non-anonymous events.
    pub topics: Vec<[u8; 32]>,
    /// Non-indexed event parameters, ABI-encoded.
    pub data: Vec<u8>,
}

/// Internal transaction: a sub-call surfaced by the EVM during the
/// outer tx's execution. `rejected = true` calls reverted; consumers
/// usually filter to non-rejected when reconstructing balance flow.
#[derive(Debug, Clone)]
pub struct InternalTx {
    /// 32-byte hash identifying this internal-tx; tree-rooted so that
    /// the root internal-tx hash equals the outer tx id.
    pub hash: [u8; 32],
    /// 21-byte address that initiated this sub-call.
    pub caller: Address,
    /// 21-byte recipient. For pure CALL with no value transfer this is
    /// still set (the callee).
    pub transfer_to: Address,
    /// Value transfers attached to this sub-call. Empty Vec is the
    /// common case — most internal calls don't move TRX.
    pub call_values: Vec<CallValueInfo>,
    /// True iff this sub-call reverted. The outer tx may still have
    /// succeeded if the revert was caught by a parent frame's try/catch.
    pub rejected: bool,
    /// Free-form note java-tron attaches (typically the precompile or
    /// opcode tag, e.g. "call", "create", "suicide"). Empty for most.
    pub note: Vec<u8>,
}

/// One entry of [`InternalTx::call_values`]. `token_id == ""` means
/// TRX; otherwise the value is denominated in the named TRC-10 token.
#[derive(Debug, Clone)]
pub struct CallValueInfo {
    pub call_value: i64,
    pub token_id: String,
}

/// Resource accounting for a tx's execution. The fields mirror
/// java-tron's `ResourceReceipt`; we keep them all because consumers
/// computing TRX cost or staking efficiency want the full breakdown.
#[derive(Debug, Clone, Default)]
pub struct ResourceCost {
    /// Energy spent by this tx (excluding origin energy).
    pub energy_usage: i64,
    /// Energy paid for in TRX (sun) when staked energy was insufficient.
    pub energy_fee: i64,
    /// Energy donated by the contract owner via origin-energy-limit.
    pub origin_energy_usage: i64,
    /// Total energy: `energy_usage + origin_energy_usage`. Pre-computed
    /// by java-tron; we surface it rather than recompute.
    pub energy_usage_total: i64,
    /// Bandwidth (net) bytes consumed.
    pub net_usage: i64,
    /// Bandwidth fee in sun (when staked bandwidth was insufficient).
    pub net_fee: i64,
    /// Penalty energy charged when contracts violate energy limits.
    /// Mostly zero on mainnet; surfaced for completeness.
    pub energy_penalty_total: i64,
}

/// Decode a `TransactionInfo` from its protobuf wire bytes. For the
/// bulk path (`GetTransactionInfoByBlockNum`) use
/// [`decode_transaction_info_list`].
pub fn decode_transaction_info(raw: &[u8]) -> Result<DecodedTxInfo> {
    let info = protocol::TransactionInfo::decode(raw)?;
    decode_transaction_info_message(&info)
}

/// Decode an already-parsed `TransactionInfo` prost message. Useful
/// when the source layer hands us the message directly off a gRPC stub.
pub fn decode_transaction_info_message(info: &protocol::TransactionInfo) -> Result<DecodedTxInfo> {
    let tx_hash = bytes_to_tx_hash(&info.id, "TransactionInfo.id")?;

    // contract_address is 21 bytes for contract txs and empty for
    // non-contract txs. Surface as Option to make the distinction
    // explicit at the type level.
    let contract_address = if info.contract_address.is_empty() {
        None
    } else {
        Some(bytes_to_address(
            &info.contract_address,
            "TransactionInfo.contract_address",
        )?)
    };

    let resource = info
        .receipt
        .as_ref()
        .map(decode_resource)
        .unwrap_or_default();

    let logs = info
        .log
        .iter()
        .map(decode_log)
        .collect::<Result<Vec<_>>>()?;

    let internal_transactions = info
        .internal_transactions
        .iter()
        .map(decode_internal)
        .collect::<Result<Vec<_>>>()?;

    // SUCCESS = 0 in the proto enum, FAILED = 1. We don't surface the
    // numeric — booleans are what consumers actually want.
    let success = info.result == protocol::transaction_info::Code::Sucess as i32;

    Ok(DecodedTxInfo {
        tx_hash,
        block_height: u64::try_from(info.block_number).unwrap_or(0),
        block_timestamp_ms: info.block_time_stamp,
        fee_sun: info.fee,
        success,
        contract_address,
        resource,
        logs,
        internal_transactions,
    })
}

/// Decode a `TransactionInfoList` (the result shape of
/// `GetTransactionInfoByBlockNum`) into per-tx [`DecodedTxInfo`].
pub fn decode_transaction_info_list(raw: &[u8]) -> Result<Vec<DecodedTxInfo>> {
    let list = protocol::TransactionInfoList::decode(raw)?;
    list.transaction_info
        .iter()
        .map(decode_transaction_info_message)
        .collect()
}

fn decode_resource(r: &protocol::ResourceReceipt) -> ResourceCost {
    ResourceCost {
        energy_usage: r.energy_usage,
        energy_fee: r.energy_fee,
        origin_energy_usage: r.origin_energy_usage,
        energy_usage_total: r.energy_usage_total,
        net_usage: r.net_usage,
        net_fee: r.net_fee,
        energy_penalty_total: r.energy_penalty_total,
    }
}

fn decode_log(l: &protocol::transaction_info::Log) -> Result<Log> {
    let address = bytes_to_address(&l.address, "Log.address")?;

    // Topics are EVM 32-byte words. java-tron occasionally emits a
    // shorter byte slice on malformed events (saw 0-byte and 31-byte
    // in mainnet samples) — we left-pad with zero rather than error,
    // matching geth's lenient behavior. Strict callers can re-check.
    let topics = l
        .topics
        .iter()
        .map(|t| {
            let mut topic = [0u8; 32];
            let n = t.len().min(32);
            topic[32 - n..].copy_from_slice(&t[..n]);
            topic
        })
        .collect();

    Ok(Log {
        address,
        topics,
        data: l.data.clone(),
    })
}

fn decode_internal(i: &protocol::InternalTransaction) -> Result<InternalTx> {
    let hash = bytes_to_hash32(&i.hash, "InternalTransaction.hash")?;
    let caller = bytes_to_address(&i.caller_address, "InternalTransaction.caller_address")?;
    let transfer_to = bytes_to_address(
        &i.transfer_to_address,
        "InternalTransaction.transferTo_address",
    )?;
    let call_values = i
        .call_value_info
        .iter()
        .map(|c| CallValueInfo {
            call_value: c.call_value,
            token_id: c.token_id.clone(),
        })
        .collect();

    Ok(InternalTx {
        hash,
        caller,
        transfer_to,
        call_values,
        rejected: i.rejected,
        note: i.note.clone(),
    })
}

fn bytes_to_address(b: &[u8], field: &'static str) -> Result<Address> {
    if b.len() != 21 {
        return Err(CodecError::BadAddress {
            field,
            got: b.len(),
        });
    }
    let mut a = [0u8; 21];
    a.copy_from_slice(b);
    Ok(Address(a))
}

fn bytes_to_tx_hash(b: &[u8], field: &'static str) -> Result<TxHash> {
    Ok(TxHash(bytes_to_hash32(b, field)?))
}

fn bytes_to_hash32(b: &[u8], field: &'static str) -> Result<[u8; 32]> {
    if b.len() != 32 {
        return Err(CodecError::BadHash {
            field,
            got: b.len(),
        });
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(b);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_proto::tron::protocol::{
        internal_transaction::CallValueInfo as WireCallValueInfo, transaction_info::Code,
        transaction_info::Log as WireLog, InternalTransaction, ResourceReceipt, TransactionInfo,
        TransactionInfoList,
    };

    fn synth_tx_info(seed: u8, with_contract: bool) -> TransactionInfo {
        TransactionInfo {
            id: vec![seed; 32],
            fee: 1_000_000,
            block_number: 82_000_000,
            block_time_stamp: 1_777_854_558_000,
            contract_result: vec![],
            contract_address: if with_contract {
                vec![0x41; 21]
            } else {
                vec![]
            },
            receipt: Some(ResourceReceipt {
                energy_usage: 11_000,
                energy_fee: 0,
                origin_energy_usage: 0,
                energy_usage_total: 11_000,
                net_usage: 268,
                net_fee: 0,
                result: 0,
                energy_penalty_total: 0,
            }),
            log: vec![WireLog {
                address: vec![0x41; 21],
                topics: vec![vec![0xaa; 32], vec![0xbb; 32]],
                data: vec![0x01, 0x02, 0x03],
            }],
            result: Code::Sucess as i32,
            res_message: vec![],
            asset_issue_id: String::new(),
            withdraw_amount: 0,
            unfreeze_amount: 0,
            internal_transactions: vec![InternalTransaction {
                hash: vec![seed.wrapping_add(1); 32],
                caller_address: vec![0x41; 21],
                transfer_to_address: vec![0x41; 21],
                call_value_info: vec![WireCallValueInfo {
                    call_value: 42,
                    token_id: String::new(),
                }],
                note: b"call".to_vec(),
                rejected: false,
                extra: String::new(),
            }],
            exchange_received_amount: 0,
            exchange_inject_another_amount: 0,
            exchange_withdraw_another_amount: 0,
            exchange_id: 0,
            shielded_transaction_fee: 0,
            order_id: vec![],
            order_details: vec![],
            packing_fee: 0,
            withdraw_expire_amount: 0,
            cancel_unfreeze_v2_amount: Default::default(),
        }
    }

    #[test]
    fn round_trips_synthetic_tx_info() {
        let wire = synth_tx_info(0x42, true);
        let bytes = wire.encode_to_vec();
        let decoded = decode_transaction_info(&bytes).expect("decode");

        assert_eq!(decoded.tx_hash.0, [0x42; 32]);
        assert_eq!(decoded.block_height, 82_000_000);
        assert_eq!(decoded.fee_sun, 1_000_000);
        assert!(decoded.success);
        assert_eq!(decoded.contract_address.unwrap().0, [0x41; 21]);
        assert_eq!(decoded.resource.energy_usage_total, 11_000);
        assert_eq!(decoded.resource.net_usage, 268);
        assert_eq!(decoded.logs.len(), 1);
        assert_eq!(decoded.logs[0].topics.len(), 2);
        assert_eq!(decoded.logs[0].topics[0], [0xaa; 32]);
        assert_eq!(decoded.logs[0].data, vec![0x01, 0x02, 0x03]);
        assert_eq!(decoded.internal_transactions.len(), 1);
        assert_eq!(decoded.internal_transactions[0].call_values[0].call_value, 42);
    }

    #[test]
    fn non_contract_tx_info_has_no_contract_address() {
        let wire = synth_tx_info(0x77, false);
        let bytes = wire.encode_to_vec();
        let decoded = decode_transaction_info(&bytes).unwrap();
        assert!(decoded.contract_address.is_none());
    }

    #[test]
    fn failed_tx_info_surfaces_success_false() {
        let mut wire = synth_tx_info(0x33, true);
        wire.result = Code::Failed as i32;
        let bytes = wire.encode_to_vec();
        let decoded = decode_transaction_info(&bytes).unwrap();
        assert!(!decoded.success);
    }

    #[test]
    fn list_decoder_handles_empty_block() {
        let list = TransactionInfoList {
            transaction_info: vec![],
        };
        let bytes = list.encode_to_vec();
        let decoded = decode_transaction_info_list(&bytes).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn list_decoder_returns_each_in_order() {
        let list = TransactionInfoList {
            transaction_info: vec![synth_tx_info(0x01, true), synth_tx_info(0x02, true)],
        };
        let bytes = list.encode_to_vec();
        let decoded = decode_transaction_info_list(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].tx_hash.0, [0x01; 32]);
        assert_eq!(decoded[1].tx_hash.0, [0x02; 32]);
    }

    #[test]
    fn short_topic_left_pads_to_32() {
        let mut wire = synth_tx_info(0x55, true);
        wire.log[0].topics = vec![vec![0xff, 0xee]]; // only 2 bytes
        let bytes = wire.encode_to_vec();
        let decoded = decode_transaction_info(&bytes).unwrap();
        let mut expected = [0u8; 32];
        expected[30] = 0xff;
        expected[31] = 0xee;
        assert_eq!(decoded.logs[0].topics[0], expected);
    }

    #[test]
    fn rejects_malformed_log_address() {
        let mut wire = synth_tx_info(0xab, true);
        wire.log[0].address = vec![0x41; 5]; // wrong length
        let bytes = wire.encode_to_vec();
        let err = decode_transaction_info(&bytes).unwrap_err();
        assert!(matches!(err, CodecError::BadAddress { .. }));
    }
}
