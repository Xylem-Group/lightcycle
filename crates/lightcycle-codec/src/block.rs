//! Block-side codec: take raw protobuf bytes, return `DecodedBlock`.
//!
//! This is the hot path — the relayer calls it on every received block
//! during sync and at chain head. Performance budget at v0.1: aim for
//! the B3 microbench hypothesis (≤ 200 µs per typical block on the
//! reference hardware in BENCHMARKS.md). Heavy operations (sigverify,
//! event ABI decoding) live behind separate entry points the relayer
//! invokes only when it has the inputs (an SR set, an ABI registry).

use prost::Message;
use sha2::{Digest, Sha256};

use lightcycle_proto::tron::protocol;
use lightcycle_types::{Address, BlockHeight, BlockId};

use crate::error::{CodecError, Result};
use crate::transaction::{decode_transaction, DecodedTransaction};

/// A block decoded from java-tron's wire format. The header is fully
/// expanded; transactions are individually decoded but the contract
/// payloads (the `Any` parameter on each `Contract`) are NOT unwrapped
/// here — that happens lazily via `decode_contract_payload` when a
/// consumer actually wants the typed contract message.
#[derive(Debug, Clone)]
pub struct DecodedBlock {
    pub header: DecodedHeader,
    pub transactions: Vec<DecodedTransaction>,
}

/// Block header in domain-friendly form.
#[derive(Debug, Clone)]
pub struct DecodedHeader {
    /// `BlockHeader.raw_data.number`. Cast from i64; TRON's height is
    /// always non-negative in practice.
    pub height: BlockHeight,
    /// Sha256 of the encoded `raw_data`. Stable identifier; matches
    /// java-tron's `blockID` concatenation of (height-prefixed) hash.
    pub block_id: BlockId,
    /// Parent block's id.
    pub parent_id: BlockId,
    /// `raw_data.tx_trie_root` — Merkle root of the transactions list.
    pub tx_trie_root: [u8; 32],
    /// Block timestamp (ms since epoch).
    pub timestamp_ms: i64,
    /// Witness (super representative) that produced this block.
    pub witness: Address,
    /// Witness signature over the raw_data bytes. Recovery + SR-set
    /// match happens elsewhere (`verify_witness_signature`, future).
    pub witness_signature: Vec<u8>,
    /// `raw_data.version` — protocol version tag, surfaced for upgrade
    /// detection.
    pub version: i32,
}

/// Decode a block from its protobuf wire representation. Does NOT
/// verify the witness signature or unwrap contract `Any` payloads;
/// see crate docs for what's deferred to specialized entry points.
pub fn decode_block(raw: &[u8]) -> Result<DecodedBlock> {
    let block = protocol::Block::decode(raw)?;
    decode_block_message(&block)
}

/// Decode a block from an already-parsed prost message. Useful when
/// the caller already has the message in hand (e.g. came in via the
/// gRPC client stub).
pub fn decode_block_message(block: &protocol::Block) -> Result<DecodedBlock> {
    let header = decode_header(
        block
            .block_header
            .as_ref()
            .ok_or(CodecError::MissingBlockHeader)?,
    )?;

    let transactions = block
        .transactions
        .iter()
        .map(decode_transaction)
        .collect::<Result<Vec<_>>>()?;

    Ok(DecodedBlock {
        header,
        transactions,
    })
}

fn decode_header(h: &protocol::BlockHeader) -> Result<DecodedHeader> {
    let raw = h.raw_data.as_ref().ok_or(CodecError::MissingHeaderRaw)?;

    // Block ID = sha256(raw_data). Re-encoding here matches what
    // java-tron does internally for the block-id derivation; the
    // canonical 32-byte hash is what peers use to dedupe.
    let raw_bytes = raw.encode_to_vec();
    let block_id_bytes = Sha256::digest(&raw_bytes);

    let parent_id = bytes_to_block_id(&raw.parent_hash, "parent_hash")?;
    let tx_trie_root = bytes_to_hash32(&raw.tx_trie_root, "tx_trie_root")?;
    let witness = bytes_to_address(&raw.witness_address, "witness_address")?;

    let mut block_id_arr = [0u8; 32];
    block_id_arr.copy_from_slice(&block_id_bytes);

    Ok(DecodedHeader {
        height: u64::try_from(raw.number).unwrap_or(0),
        block_id: BlockId(block_id_arr),
        parent_id,
        tx_trie_root,
        timestamp_ms: raw.timestamp,
        witness,
        witness_signature: h.witness_signature.clone(),
        version: raw.version,
    })
}

fn bytes_to_block_id(b: &[u8], field: &'static str) -> Result<BlockId> {
    Ok(BlockId(bytes_to_hash32(b, field)?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_proto::tron::protocol::block_header::Raw as RawHeader;
    use lightcycle_proto::tron::protocol::transaction::contract::ContractType;
    use lightcycle_proto::tron::protocol::transaction::{Contract, Raw as RawTx};
    use lightcycle_proto::tron::protocol::{Block, BlockHeader, Transaction};

    /// Build a syntactically valid block at a given height with `n`
    /// trivial transactions. Useful for tests + benchmarks; not
    /// representative of mainnet density.
    pub(crate) fn synth_block(height: i64, n_txs: usize) -> Vec<u8> {
        let raw_header = RawHeader {
            timestamp: 1_777_854_558_000,
            tx_trie_root: vec![0xab; 32],
            parent_hash: vec![0xcd; 32],
            number: height,
            witness_id: 0,
            witness_address: vec![0x41; 21],
            version: 34,
            account_state_root: vec![0xef; 32],
        };
        let header = BlockHeader {
            raw_data: Some(raw_header),
            witness_signature: vec![0x99; 65],
        };

        let txs: Vec<Transaction> = (0..n_txs)
            .map(|i| Transaction {
                raw_data: Some(RawTx {
                    ref_block_bytes: vec![],
                    ref_block_num: 0,
                    ref_block_hash: vec![],
                    expiration: 1_777_854_558_000 + 60_000,
                    auths: vec![],
                    data: vec![],
                    contract: vec![Contract {
                        r#type: ContractType::TransferContract as i32,
                        parameter: None,
                        provider: vec![],
                        contract_name: vec![],
                        permission_id: 0,
                    }],
                    scripts: vec![],
                    timestamp: 1_777_854_558_000 + i as i64,
                    fee_limit: 0,
                }),
                signature: vec![vec![0x77; 65]],
                ret: vec![],
            })
            .collect();

        let block = Block {
            transactions: txs,
            block_header: Some(header),
        };
        block.encode_to_vec()
    }

    #[test]
    fn decodes_synthetic_empty_block() {
        let bytes = synth_block(82_500_000, 0);
        let decoded = decode_block(&bytes).expect("decode");
        assert_eq!(decoded.header.height, 82_500_000);
        assert_eq!(decoded.header.timestamp_ms, 1_777_854_558_000);
        assert_eq!(decoded.transactions.len(), 0);
    }

    #[test]
    fn decodes_synthetic_block_with_transfers() {
        let bytes = synth_block(82_500_001, 200);
        let decoded = decode_block(&bytes).expect("decode");
        assert_eq!(decoded.transactions.len(), 200);
        assert!(decoded
            .transactions
            .iter()
            .all(|tx| tx.contracts == vec![crate::transaction::ContractKind::Transfer]));
        // Each tx has a unique timestamp → unique hash → no duplicates.
        let mut hashes: Vec<_> = decoded.transactions.iter().map(|t| t.hash).collect();
        hashes.sort_by(|a, b| a.0.cmp(&b.0));
        let n_before = hashes.len();
        hashes.dedup();
        assert_eq!(hashes.len(), n_before);
    }

    #[test]
    fn header_block_id_is_stable_across_decode() {
        let bytes = synth_block(82_500_002, 5);
        let a = decode_block(&bytes).unwrap();
        let b = decode_block(&bytes).unwrap();
        assert_eq!(a.header.block_id, b.header.block_id);
        assert_eq!(a.header.parent_id, b.header.parent_id);
    }

    #[test]
    fn rejects_block_without_header() {
        let block = Block {
            transactions: vec![],
            block_header: None,
        };
        let err = decode_block(&block.encode_to_vec()).unwrap_err();
        assert!(matches!(err, CodecError::MissingBlockHeader));
    }
}
