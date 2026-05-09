//! Topic-recognized decoders for the two universal TRC-20 events.
//!
//! TRC-20 inherits its event ABI from ERC-20: `Transfer(address,address,uint256)`
//! and `Approval(address,address,uint256)`. Every TRC-20 token on
//! TRON emits these with the same indexed topic 0, so we can decode
//! them without an ABI registry — the topic hash *is* the schema.
//!
//! For everything else (token-specific events, TRC-721/1155, custom
//! contract events) consumers need an ABI registry; those don't go
//! here. v0.1 ships only the universal pair.
//!
//! TRC-721 note: `Transfer(address indexed from, address indexed to,
//! uint256 indexed tokenId)` shares topic[0] with TRC-20 Transfer
//! but has 4 topics (tokenId is indexed). [`decode_trc20_transfer`]
//! returns `None` on 4-topic logs, so 721 transfers fall through.
//! A separate `decode_trc721_transfer` will land alongside the rest
//! of TRC-721 support.

use lightcycle_types::Address;

use crate::tx_info::Log;

/// `keccak256("Transfer(address,address,uint256)")` — emitted by every
/// TRC-20 contract on transfer.
pub const TRC20_TRANSFER_TOPIC0: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];

/// `keccak256("Approval(address,address,uint256)")` — emitted by every
/// TRC-20 contract on approve.
pub const TRC20_APPROVAL_TOPIC0: [u8; 32] = [
    0x8c, 0x5b, 0xe1, 0xe5, 0xeb, 0xec, 0x7d, 0x5b, 0xd1, 0x4f, 0x71, 0x42, 0x7d, 0x1e, 0x84, 0xf3,
    0xdd, 0x03, 0x14, 0xc0, 0xf7, 0xb2, 0x29, 0x1e, 0x5b, 0x20, 0x0a, 0xc8, 0xc7, 0xc3, 0xb9, 0x25,
];

/// Decoded TRC-20 `Transfer` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trc20Transfer {
    /// The token contract that emitted the event (21-byte TRON address).
    pub token: Address,
    /// `from` (21-byte TRON address; the EVM topic is 20-byte, we
    /// re-prepend the 0x41 network byte).
    pub from: Address,
    /// `to` (21-byte TRON address).
    pub to: Address,
    /// `value` as raw 32-byte big-endian uint256. TRC-20 amounts often
    /// exceed u128 once you account for 18-decimal tokens with whale
    /// balances, so we don't presume a smaller integer type. Convert
    /// with the consumer's preferred bigint crate (`U256`, `BigUint`,
    /// `ruint`, …).
    pub value: [u8; 32],
}

/// Decoded TRC-20 `Approval` event. Mirrors [`Trc20Transfer`] but
/// names the parties `owner` / `spender`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trc20Approval {
    pub token: Address,
    pub owner: Address,
    pub spender: Address,
    pub value: [u8; 32],
}

/// Try to decode a [`Log`] as a TRC-20 `Transfer`. Returns `None` if
/// the topic hash doesn't match, the topic count is wrong (TRC-721
/// transfers have 4 topics), or the data length is wrong.
///
/// We intentionally don't error on a malformed candidate: a
/// non-TRC-20 contract is allowed to emit a log with the same topic
/// hash but a different shape. We just decline to interpret it.
pub fn decode_trc20_transfer(log: &Log) -> Option<Trc20Transfer> {
    if log.topics.len() != 3 || log.topics[0] != TRC20_TRANSFER_TOPIC0 {
        return None;
    }
    let from = topic_to_tron_address(&log.topics[1])?;
    let to = topic_to_tron_address(&log.topics[2])?;
    let value = data_to_uint256(&log.data)?;
    Some(Trc20Transfer {
        token: log.address,
        from,
        to,
        value,
    })
}

/// Try to decode a [`Log`] as a TRC-20 `Approval`. Same shape as
/// [`decode_trc20_transfer`], same lenient `None`-on-mismatch behavior.
pub fn decode_trc20_approval(log: &Log) -> Option<Trc20Approval> {
    if log.topics.len() != 3 || log.topics[0] != TRC20_APPROVAL_TOPIC0 {
        return None;
    }
    let owner = topic_to_tron_address(&log.topics[1])?;
    let spender = topic_to_tron_address(&log.topics[2])?;
    let value = data_to_uint256(&log.data)?;
    Some(Trc20Approval {
        token: log.address,
        owner,
        spender,
        value,
    })
}

/// EVM topics encode addresses as 32-byte left-padded values. The
/// last 20 bytes are the address proper; everything before should be
/// zero. We don't enforce that — geth doesn't either — and just take
/// the trailing 20 bytes, prepending 0x41 to form the TRON address.
fn topic_to_tron_address(topic: &[u8; 32]) -> Option<Address> {
    let mut out = [0u8; 21];
    out[0] = 0x41;
    out[1..].copy_from_slice(&topic[12..]);
    Some(Address(out))
}

fn data_to_uint256(data: &[u8]) -> Option<[u8; 32]> {
    if data.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(data);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic_with_address(prefix_zero: bool, addr20: [u8; 20]) -> [u8; 32] {
        let mut t = [0u8; 32];
        if prefix_zero {
            t[12..].copy_from_slice(&addr20);
        } else {
            // EVM tolerates non-zero junk in the high 12 bytes; we
            // exercise that path.
            for b in t.iter_mut().take(12) {
                *b = 0xa5;
            }
            t[12..].copy_from_slice(&addr20);
        }
        t
    }

    fn synth_transfer_log() -> Log {
        let from20 = [0x11; 20];
        let to20 = [0x22; 20];
        let mut value = [0u8; 32];
        value[24..].copy_from_slice(&1_000_000u64.to_be_bytes());
        Log {
            address: Address([
                0x41, 0xa6, 0x14, 0xf8, 0x03, 0xb6, 0xfd, 0x78, 0x09, 0x86, 0xa4, 0x2c, 0x78, 0xf9,
                0xe5, 0x07, 0x80, 0xa9, 0x6f, 0xed, 0xa3,
            ]),
            topics: vec![
                TRC20_TRANSFER_TOPIC0,
                topic_with_address(true, from20),
                topic_with_address(true, to20),
            ],
            data: value.to_vec(),
        }
    }

    #[test]
    fn decodes_clean_transfer() {
        let log = synth_transfer_log();
        let t = decode_trc20_transfer(&log).expect("decode");
        assert_eq!(t.from.0[0], 0x41);
        assert_eq!(&t.from.0[1..], &[0x11; 20]);
        assert_eq!(&t.to.0[1..], &[0x22; 20]);
        let mut expected_val = [0u8; 32];
        expected_val[24..].copy_from_slice(&1_000_000u64.to_be_bytes());
        assert_eq!(t.value, expected_val);
    }

    #[test]
    fn returns_none_on_wrong_topic_count() {
        let mut log = synth_transfer_log();
        log.topics.push([0x99; 32]); // 4 topics — looks like TRC-721
        assert!(decode_trc20_transfer(&log).is_none());
    }

    #[test]
    fn returns_none_on_wrong_topic0() {
        let mut log = synth_transfer_log();
        log.topics[0] = [0xff; 32];
        assert!(decode_trc20_transfer(&log).is_none());
    }

    #[test]
    fn returns_none_on_short_data() {
        let mut log = synth_transfer_log();
        log.data.truncate(16);
        assert!(decode_trc20_transfer(&log).is_none());
    }

    #[test]
    fn topic_dirty_high_bytes_still_decode() {
        // Some contracts (or buggy proxies) emit topics with junk in
        // the high 12 bytes. We treat the trailing 20 as the address
        // and don't reject — match geth's permissive behavior.
        let from20 = [0x77; 20];
        let to20 = [0x88; 20];
        let mut value = [0u8; 32];
        value[31] = 1;
        let log = Log {
            address: Address([0x41; 21]),
            topics: vec![
                TRC20_TRANSFER_TOPIC0,
                topic_with_address(false, from20),
                topic_with_address(false, to20),
            ],
            data: value.to_vec(),
        };
        let t = decode_trc20_transfer(&log).unwrap();
        assert_eq!(&t.from.0[1..], &[0x77; 20]);
        assert_eq!(&t.to.0[1..], &[0x88; 20]);
    }

    #[test]
    fn decodes_approval() {
        let mut log = synth_transfer_log();
        log.topics[0] = TRC20_APPROVAL_TOPIC0;
        let a = decode_trc20_approval(&log).expect("decode");
        assert_eq!(&a.owner.0[1..], &[0x11; 20]);
        assert_eq!(&a.spender.0[1..], &[0x22; 20]);
    }

    #[test]
    fn topic0_constants_match_keccak256_of_event_signature() {
        // The bytes baked into the constants are the only place this
        // codec gets the topic hash from. If the constants are wrong,
        // every TRC-20 event in the world will silently fall through
        // as "not a Transfer/Approval." Recompute them here from the
        // canonical signature string so the test catches a typo.
        use sha3::{Digest, Keccak256};
        let transfer = Keccak256::digest(b"Transfer(address,address,uint256)");
        let approval = Keccak256::digest(b"Approval(address,address,uint256)");
        assert_eq!(&transfer[..], &TRC20_TRANSFER_TOPIC0);
        assert_eq!(&approval[..], &TRC20_APPROVAL_TOPIC0);
    }
}
