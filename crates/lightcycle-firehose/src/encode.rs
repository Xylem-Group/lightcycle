//! Encode `BufferedBlock` (from the relayer's reorg engine) into the
//! `sf.tron.type.v1.Block` wire shape that ships on Firehose `Response.block`.
//!
//! The encoder is pure: it takes a reference to the in-memory decoded
//! block and produces a freshly-allocated proto message. No I/O, no
//! async, no error returns — every input shape is encodable.
//!
//! What's populated:
//!
//! - **Block + Header + Transaction + Contract**: from `DecodedBlock`.
//!   The four high-volume contract types (`Transfer`, `TransferAsset`,
//!   `TriggerSmartContract`, `CreateSmartContract`) get their typed
//!   payload variants; everything else lands in the `OtherPayload`
//!   oneof variant with the original `Any.value` bytes preserved + the
//!   wire `kind` tag in `Contract.unknown_kind` so consumers can route
//!   to the right java-tron-specific decoder.
//!
//! - **Transaction.info**: from `BufferedBlock.tx_infos`, joined by
//!   tx hash. The TRC-20 / TRC-721 indexing path needs `info.logs`;
//!   resource-cost accounting reads `info.resource`. When the upstream
//!   `getTransactionInfoByBlockNum` RPC failed (or the relayer ran
//!   with `--no-fetch-tx-info`), `tx_infos` is empty and every
//!   `Transaction.info` is `None` — same observable shape as before
//!   tx-info ingest landed.
//!
//! Tx-info ordering is NOT guaranteed to match the block's tx order
//! on the wire, so the encoder builds a hash-map by tx hash and
//! removes entries as it joins. A tx in the block with no matching
//! info gets `info = None`. An info with no matching tx in the block
//! is currently dropped silently (never observed in mainnet samples,
//! but logging the rate would help if it ever happens).

use std::collections::HashMap;

use lightcycle_codec::{DecodedContract, DecodedTransaction, DecodedTxInfo};
use lightcycle_proto::sf::tron::type_v1 as pb;
use lightcycle_relayer::BufferedBlock;
use lightcycle_types::TxHash;
use prost_types::Timestamp;

/// Encode a buffered block into `sf.tron.type.v1.Block`.
pub fn encode_block(buffered: &BufferedBlock) -> pb::Block {
    let header = &buffered.decoded.header;

    // Build a lookup keyed by tx hash so the per-tx encoder can join
    // its `info` in O(1). HashMap rather than sort-and-binary-search:
    // mainnet block density (10–500 txs) makes the constant factor
    // win out and the code stays simpler.
    let info_by_hash: HashMap<TxHash, &DecodedTxInfo> = buffered
        .tx_infos
        .iter()
        .map(|i| (i.tx_hash, i))
        .collect();

    pb::Block {
        number: buffered.height,
        id: buffered.block_id.0.to_vec(),
        parent_id: buffered.parent_id.0.to_vec(),
        time: Some(timestamp_from_ms(header.timestamp_ms)),
        header: Some(pb::Header {
            version: header.version,
            raw_data_hash: header.raw_data_hash.to_vec(),
            tx_trie_root: header.tx_trie_root.to_vec(),
            witness: header.witness.0.to_vec(),
            witness_signature: header.witness_signature.clone(),
        }),
        transactions: buffered
            .decoded
            .transactions
            .iter()
            .map(|tx| encode_transaction(tx, info_by_hash.get(&tx.hash).copied()))
            .collect(),
    }
}

fn encode_transaction(
    tx: &DecodedTransaction,
    info: Option<&DecodedTxInfo>,
) -> pb::Transaction {
    pb::Transaction {
        id: tx.hash.0.to_vec(),
        timestamp_ms: tx.timestamp_ms,
        expiration_ms: tx.expiration_ms,
        fee_limit_sun: tx.fee_limit_sun,
        contracts: tx.contracts.iter().map(encode_contract).collect(),
        signatures: tx.signatures.clone(),
        info: info.map(encode_tx_info),
    }
}

fn encode_tx_info(info: &DecodedTxInfo) -> pb::TransactionInfo {
    let resource = pb::ResourceCost {
        energy_usage: info.resource.energy_usage,
        energy_fee: info.resource.energy_fee,
        origin_energy_usage: info.resource.origin_energy_usage,
        energy_usage_total: info.resource.energy_usage_total,
        net_usage: info.resource.net_usage,
        net_fee: info.resource.net_fee,
        energy_penalty_total: info.resource.energy_penalty_total,
    };
    pb::TransactionInfo {
        success: info.success,
        fee_sun: info.fee_sun,
        contract_address: info
            .contract_address
            .map(|a| a.0.to_vec())
            .unwrap_or_default(),
        resource: Some(resource),
        logs: info
            .logs
            .iter()
            .map(|l| pb::Log {
                address: l.address.0.to_vec(),
                topics: l.topics.iter().map(|t| t.to_vec()).collect(),
                data: l.data.clone(),
            })
            .collect(),
        internal_transactions: info
            .internal_transactions
            .iter()
            .map(|i| pb::InternalTx {
                hash: i.hash.to_vec(),
                caller: i.caller.0.to_vec(),
                transfer_to: i.transfer_to.0.to_vec(),
                call_values: i
                    .call_values
                    .iter()
                    .map(|c| pb::CallValue {
                        call_value: c.call_value,
                        token_id: c.token_id.clone(),
                    })
                    .collect(),
                rejected: i.rejected,
                note: i.note.clone(),
            })
            .collect(),
    }
}

fn encode_contract(c: &DecodedContract) -> pb::Contract {
    use pb::contract::Payload;
    use pb::ContractKind as PbKind;

    match c {
        DecodedContract::Transfer { owner, to, amount_sun } => pb::Contract {
            kind: PbKind::Transfer as i32,
            unknown_kind: 0,
            payload: Some(Payload::Transfer(pb::Transfer {
                owner: owner.0.to_vec(),
                to: to.0.to_vec(),
                amount_sun: *amount_sun,
            })),
        },
        DecodedContract::TransferAsset {
            owner,
            to,
            asset_name,
            amount,
        } => pb::Contract {
            kind: PbKind::TransferAsset as i32,
            unknown_kind: 0,
            payload: Some(Payload::TransferAsset(pb::TransferAsset {
                owner: owner.0.to_vec(),
                to: to.0.to_vec(),
                asset_name: asset_name.clone(),
                amount: *amount,
            })),
        },
        DecodedContract::TriggerSmartContract {
            owner,
            contract_address,
            call_value,
            call_token_value,
            token_id,
            data,
        } => pb::Contract {
            kind: PbKind::TriggerSmartContract as i32,
            unknown_kind: 0,
            payload: Some(Payload::TriggerSmartContract(pb::TriggerSmartContract {
                owner: owner.0.to_vec(),
                contract_address: contract_address.0.to_vec(),
                data: data.clone(),
                call_value_sun: *call_value,
                call_token_value: *call_token_value,
                token_id: *token_id,
            })),
        },
        DecodedContract::CreateSmartContract {
            owner,
            contract_address,
            bytecode,
            name,
            consume_user_resource_percent,
            origin_energy_limit,
            call_token_value,
            token_id,
        } => pb::Contract {
            kind: PbKind::CreateSmartContract as i32,
            unknown_kind: 0,
            payload: Some(Payload::CreateSmartContract(pb::CreateSmartContract {
                owner: owner.0.to_vec(),
                contract_address: contract_address
                    .map(|a| a.0.to_vec())
                    .unwrap_or_default(),
                name: name.clone(),
                bytecode: bytecode.clone(),
                consume_user_resource_percent: *consume_user_resource_percent,
                origin_energy_limit: *origin_energy_limit,
                call_token_value: *call_token_value,
                token_id: *token_id,
            })),
        },
        DecodedContract::Other { kind, raw } => pb::Contract {
            kind: PbKind::Other as i32,
            unknown_kind: kind.to_wire_tag(),
            payload: Some(Payload::OtherPayload(raw.clone())),
        },
    }
}

fn timestamp_from_ms(ms: i64) -> Timestamp {
    Timestamp {
        seconds: ms / 1000,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}

/// Type URL for the `sf.tron.type.v1.Block` payload, used on
/// Firehose `Response.block.type_url`. Substreams modules match
/// against this string when deciding which decoder to apply.
pub const BLOCK_TYPE_URL: &str = "type.googleapis.com/sf.tron.type.v1.Block";

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_codec::{ContractKind, DecodedBlock, DecodedHeader};
    use lightcycle_types::{Address, BlockId, TxHash};
    use prost::Message;

    fn addr(b: u8) -> Address {
        let mut a = [0u8; 21];
        a[0] = 0x41;
        a[1..].fill(b);
        Address(a)
    }

    fn synth_buffered(height: u64, contracts: Vec<DecodedContract>) -> BufferedBlock {
        synth_buffered_with_infos(height, contracts, vec![])
    }

    fn synth_buffered_with_infos(
        height: u64,
        contracts: Vec<DecodedContract>,
        tx_infos: Vec<DecodedTxInfo>,
    ) -> BufferedBlock {
        let tx = DecodedTransaction {
            hash: TxHash([0xaa; 32]),
            timestamp_ms: 1_777_854_558_100,
            expiration_ms: 1_777_854_618_100,
            fee_limit_sun: 1_000_000,
            contracts,
            signatures: vec![vec![0x77; 65]],
        };
        BufferedBlock {
            height,
            block_id: BlockId([height as u8; 32]),
            parent_id: BlockId([(height.saturating_sub(1)) as u8; 32]),
            fork_id: 0,
            decoded: DecodedBlock {
                header: DecodedHeader {
                    height,
                    block_id: BlockId([height as u8; 32]),
                    parent_id: BlockId([(height.saturating_sub(1)) as u8; 32]),
                    raw_data_hash: [0xbe; 32],
                    tx_trie_root: [0xab; 32],
                    timestamp_ms: 1_777_854_558_345,
                    witness: addr(0x99),
                    witness_signature: vec![0x99; 65],
                    version: 34,
                },
                transactions: vec![tx],
            },
            tx_infos,
        }
    }

    #[test]
    fn block_top_level_fields_round_trip() {
        let buf = synth_buffered(82_500_001, vec![]);
        let pb = encode_block(&buf);
        // Round-trip through prost so the field tags are exercised.
        let bytes = pb.encode_to_vec();
        let decoded = pb::Block::decode(bytes.as_slice()).expect("decode");

        assert_eq!(decoded.number, 82_500_001);
        assert_eq!(decoded.id.len(), 32);
        assert_eq!(decoded.parent_id.len(), 32);
        let t = decoded.time.expect("time");
        assert_eq!(t.seconds, 1_777_854_558);
        assert_eq!(t.nanos, 345_000_000);
        let h = decoded.header.expect("header");
        assert_eq!(h.version, 34);
        assert_eq!(h.witness.len(), 21);
        assert_eq!(h.witness_signature.len(), 65);
    }

    #[test]
    fn transfer_contract_encodes_into_typed_payload() {
        use pb::contract::Payload;
        let contracts = vec![DecodedContract::Transfer {
            owner: addr(0x10),
            to: addr(0x20),
            amount_sun: 1_234_567,
        }];
        let buf = synth_buffered(82_500_002, contracts);
        let pb = encode_block(&buf);
        let tx = &pb.transactions[0];
        let c = &tx.contracts[0];
        assert_eq!(c.kind, pb::ContractKind::Transfer as i32);
        assert_eq!(c.unknown_kind, 0);
        match c.payload.as_ref().expect("payload") {
            Payload::Transfer(t) => {
                assert_eq!(t.owner, addr(0x10).0);
                assert_eq!(t.to, addr(0x20).0);
                assert_eq!(t.amount_sun, 1_234_567);
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }

    #[test]
    fn other_contract_preserves_wire_tag_and_raw_bytes() {
        use pb::contract::Payload;
        // FreezeBalanceV2 isn't one of the four explicit variants;
        // encoder must surface the wire tag in unknown_kind and the
        // raw bytes in OtherPayload.
        let raw = vec![0x01, 0x02, 0x03];
        let contracts = vec![DecodedContract::Other {
            kind: ContractKind::FreezeBalanceV2,
            raw: raw.clone(),
        }];
        let buf = synth_buffered(82_500_003, contracts);
        let pb = encode_block(&buf);
        let c = &pb.transactions[0].contracts[0];
        assert_eq!(c.kind, pb::ContractKind::Other as i32);
        assert_eq!(c.unknown_kind, ContractKind::FreezeBalanceV2.to_wire_tag());
        match c.payload.as_ref().expect("payload") {
            Payload::OtherPayload(bytes) => assert_eq!(bytes, &raw),
            other => panic!("expected OtherPayload, got {other:?}"),
        }
    }

    #[test]
    fn unknown_wire_tag_round_trips_through_other_payload() {
        use pb::contract::Payload;
        // Forward-compat: a TRON fork that adds a new contract type
        // before we re-vendor lands as Other(tag) in the codec; the
        // encoder must preserve that exact tag on the wire.
        let contracts = vec![DecodedContract::Other {
            kind: ContractKind::Other(9999),
            raw: vec![0xff],
        }];
        let buf = synth_buffered(82_500_004, contracts);
        let pb = encode_block(&buf);
        let c = &pb.transactions[0].contracts[0];
        assert_eq!(c.unknown_kind, 9999);
        match c.payload.as_ref().expect("payload") {
            Payload::OtherPayload(bytes) => assert_eq!(bytes, &[0xff]),
            other => panic!("expected OtherPayload, got {other:?}"),
        }
    }

    #[test]
    fn empty_block_encodes_with_zero_transactions() {
        // Empty blocks are common on TRON (3-second cadence + low load
        // periods produce many) — encoder must handle them cleanly.
        let mut buf = synth_buffered(82_500_005, vec![]);
        buf.decoded.transactions.clear();
        let pb = encode_block(&buf);
        assert!(pb.transactions.is_empty());
        assert_eq!(pb.number, 82_500_005);
    }

    #[test]
    fn tx_info_unset_when_buffered_block_has_no_infos() {
        // When the relayer ran without tx-info fetching (or the RPC
        // failed), tx_infos is empty and every Transaction.info must
        // remain None — same shape as before tx-info ingest landed,
        // so consumers don't see spurious empty-but-Some payloads.
        let buf = synth_buffered(82_500_006, vec![]);
        let pb = encode_block(&buf);
        for tx in &pb.transactions {
            assert!(tx.info.is_none());
        }
    }

    #[test]
    fn tx_info_joins_by_hash_and_populates_all_fields() {
        use lightcycle_codec::{InternalTx, Log, ResourceCost};
        // tx_infos arrive in arbitrary order relative to block.transactions;
        // here the synthetic block has one tx with hash [0xaa; 32], and
        // we attach a populated DecodedTxInfo with that hash.
        let info = DecodedTxInfo {
            tx_hash: TxHash([0xaa; 32]),
            block_height: 82_500_010,
            block_timestamp_ms: 1_777_854_558_345,
            fee_sun: 7_777_777,
            success: true,
            contract_address: Some(addr(0x55)),
            resource: ResourceCost {
                energy_usage: 11_000,
                energy_fee: 0,
                origin_energy_usage: 0,
                energy_usage_total: 11_000,
                net_usage: 268,
                net_fee: 0,
                energy_penalty_total: 0,
            },
            logs: vec![Log {
                address: addr(0x55),
                topics: vec![[0xa9; 32], [0xb0; 32], [0xc1; 32]],
                data: vec![0xde, 0xad, 0xbe, 0xef],
            }],
            internal_transactions: vec![InternalTx {
                hash: [0x42; 32],
                caller: addr(0x55),
                transfer_to: addr(0x66),
                call_values: vec![lightcycle_codec::CallValueInfo {
                    call_value: 100,
                    token_id: String::new(),
                }],
                rejected: false,
                note: b"call".to_vec(),
            }],
        };
        let buf = synth_buffered_with_infos(82_500_010, vec![], vec![info.clone()]);
        let pb = encode_block(&buf);
        let pb_info = pb.transactions[0].info.as_ref().expect("info populated");
        assert!(pb_info.success);
        assert_eq!(pb_info.fee_sun, 7_777_777);
        assert_eq!(pb_info.contract_address, addr(0x55).0.to_vec());
        let res = pb_info.resource.expect("resource");
        assert_eq!(res.energy_usage_total, 11_000);
        assert_eq!(res.net_usage, 268);
        assert_eq!(pb_info.logs.len(), 1);
        assert_eq!(pb_info.logs[0].topics.len(), 3);
        assert_eq!(pb_info.logs[0].data, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(pb_info.internal_transactions.len(), 1);
        assert_eq!(pb_info.internal_transactions[0].rejected, false);
        assert_eq!(pb_info.internal_transactions[0].call_values[0].call_value, 100);
    }

    #[test]
    fn tx_with_no_matching_info_keeps_info_none() {
        // tx_infos may not cover every tx (e.g. partial fetch error).
        // Encoder must leave info=None for unmatched txs rather than
        // pulling the wrong info or panicking.
        let mismatched_info = DecodedTxInfo {
            tx_hash: TxHash([0xee; 32]), // doesn't match the block's [0xaa; 32]
            block_height: 82_500_011,
            block_timestamp_ms: 1_777_854_558_345,
            fee_sun: 0,
            success: true,
            contract_address: None,
            resource: lightcycle_codec::ResourceCost::default(),
            logs: vec![],
            internal_transactions: vec![],
        };
        let buf = synth_buffered_with_infos(82_500_011, vec![], vec![mismatched_info]);
        let pb = encode_block(&buf);
        assert!(pb.transactions[0].info.is_none());
    }

    #[test]
    fn create_smart_contract_with_empty_address_emits_empty_bytes() {
        use pb::contract::Payload;
        // Codec models contract_address as Option<Address>; when None,
        // the proto field must be empty bytes (not [0; 21]).
        let contracts = vec![DecodedContract::CreateSmartContract {
            owner: addr(0x77),
            contract_address: None,
            bytecode: vec![0x60, 0x80],
            name: "Test".into(),
            consume_user_resource_percent: 100,
            origin_energy_limit: 1_000_000,
            call_token_value: 0,
            token_id: 0,
        }];
        let buf = synth_buffered(82_500_007, contracts);
        let pb = encode_block(&buf);
        match pb.transactions[0].contracts[0]
            .payload
            .as_ref()
            .expect("payload")
        {
            Payload::CreateSmartContract(m) => {
                assert!(m.contract_address.is_empty());
                assert_eq!(m.name, "Test");
            }
            other => panic!("expected CreateSmartContract, got {other:?}"),
        }
    }
}
