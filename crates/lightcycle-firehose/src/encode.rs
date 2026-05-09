//! Encode `BufferedBlock` (from the relayer's reorg engine) into the
//! `sf.tron.type.v1.Block` wire shape that ships on Firehose `Response.block`.
//!
//! The encoder is pure: it takes a reference to the in-memory decoded
//! block and produces a freshly-allocated proto message. No I/O, no
//! async, no error returns — every input shape is encodable.
//!
//! What's populated and what's not:
//!
//! - **Block + Header + Transaction + Contract**: fully populated from
//!   `DecodedBlock`. The four high-volume contract types
//!   (`Transfer`, `TransferAsset`, `TriggerSmartContract`,
//!   `CreateSmartContract`) get their typed payload variants;
//!   everything else lands in the `OtherPayload` oneof variant with
//!   the original `Any.value` bytes preserved + the wire `kind` tag in
//!   `Contract.unknown_kind` so consumers can route to the right
//!   java-tron-specific decoder.
//!
//! - **Transaction.info**: left `None`. v0.1 of the ingest pipeline
//!   does NOT fetch java-tron's `getTransactionInfoByBlockNum` side
//!   channel, so logs / internal txs / resource accounting are unknown
//!   at this layer. When ingest is extended, this encoder gains the
//!   join responsibility — the proto shape stays unchanged so existing
//!   consumers transparently start seeing populated `info`.

use lightcycle_codec::{DecodedContract, DecodedTransaction};
use lightcycle_proto::sf::tron::type_v1 as pb;
use lightcycle_relayer::BufferedBlock;
use prost_types::Timestamp;

/// Encode a buffered block into `sf.tron.type.v1.Block`.
pub fn encode_block(buffered: &BufferedBlock) -> pb::Block {
    let header = &buffered.decoded.header;

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
            .map(encode_transaction)
            .collect(),
    }
}

fn encode_transaction(tx: &DecodedTransaction) -> pb::Transaction {
    pb::Transaction {
        id: tx.hash.0.to_vec(),
        timestamp_ms: tx.timestamp_ms,
        expiration_ms: tx.expiration_ms,
        fee_limit_sun: tx.fee_limit_sun,
        contracts: tx.contracts.iter().map(encode_contract).collect(),
        signatures: tx.signatures.clone(),
        // v0.1 ingest doesn't fetch TransactionInfo. See module docs.
        info: None,
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
    fn tx_info_field_is_unset_in_v01_pipeline() {
        // Documents the v0.1 boundary: ingest doesn't fetch
        // TransactionInfo, so the proto's `info` field must be None
        // for now. When ingest is extended, this assertion gets
        // updated.
        let buf = synth_buffered(82_500_006, vec![]);
        let pb = encode_block(&buf);
        for tx in &pb.transactions {
            assert!(tx.info.is_none(), "info should be unset in v0.1");
        }
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
