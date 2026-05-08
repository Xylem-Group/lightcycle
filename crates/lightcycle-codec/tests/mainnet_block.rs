//! Decode a real mainnet block fixture end-to-end.
//!
//! The fixture in `tests/fixtures/mainnet-head.bin` is captured by
//! running `lightcycle inspect --dump-to <path>` against a live
//! java-tron node (committed once and kept stable; refresh by
//! re-running the inspect command and replacing the file). This test
//! is the difference between "codec works on synthetic input" and
//! "codec works on bytes-in-the-wild."

use lightcycle_codec::{decode_block, ContractKind};

const MAINNET_HEAD_FIXTURE: &[u8] = include_bytes!("fixtures/mainnet-head.bin");

#[test]
fn decodes_real_mainnet_block_without_panic() {
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).expect("decode mainnet block");

    // Sanity: height is plausibly mainnet (greater than the v4.7.6 sync
    // wedge head, less than far-future). Both bounds are loose; the
    // point is to catch a fixture file that's been replaced with junk.
    assert!(
        decoded.header.height > 80_000_000,
        "height {} below sanity bound (fixture stale or junk?)",
        decoded.header.height
    );
    assert!(
        decoded.header.height < 200_000_000,
        "height {} above sanity bound (fixture from the future?)",
        decoded.header.height
    );

    // TRON addresses always start with 0x41 (the network prefix).
    assert_eq!(
        decoded.header.witness.0[0], 0x41,
        "witness address missing 0x41 network prefix — header decode broken"
    );

    // TRON's witness signature is secp256k1 ECDSA + recovery id = 65 bytes.
    assert_eq!(
        decoded.header.witness_signature.len(),
        65,
        "witness signature length unexpected"
    );

    // Block must have at least one transaction (mainnet head is busy).
    // If a future fixture happens to be a 0-tx block, relax this; the
    // smoke check below catches the same bug differently.
    assert!(
        !decoded.transactions.is_empty(),
        "no transactions — fixture from a quiet block? Replace with a busier head."
    );

    // Each tx hash must be 32 bytes (TxHash type guarantees that, but
    // also: each tx must have at least one signature and at least one
    // contract — true for any well-formed mainnet tx).
    for tx in &decoded.transactions {
        assert_eq!(tx.hash.0.len(), 32);
        assert!(!tx.signatures.is_empty());
        assert!(!tx.contracts.is_empty());
    }
}

#[test]
fn mainnet_block_has_recognized_contract_types() {
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).unwrap();

    // No `Other(_)` variant should appear in our retained contract set —
    // we vendored all 41 known mainnet contract types from java-tron
    // GreatVoyage-v4.8.1. If this fires, either upstream added a new
    // contract type after v4.8.1 (refresh the vendor) or our enum
    // mapping is broken (look at ContractKind::from_wire).
    for tx in &decoded.transactions {
        for c in &tx.contracts {
            assert!(
                !matches!(c, ContractKind::Other(_)),
                "unrecognized contract tag {:?} — vendor refresh needed?",
                c
            );
        }
    }
}

#[test]
fn mainnet_block_id_is_deterministic() {
    let a = decode_block(MAINNET_HEAD_FIXTURE).unwrap();
    let b = decode_block(MAINNET_HEAD_FIXTURE).unwrap();
    assert_eq!(a.header.block_id, b.header.block_id);
    assert_eq!(a.header.height, b.header.height);
    assert_eq!(a.transactions.len(), b.transactions.len());
}
