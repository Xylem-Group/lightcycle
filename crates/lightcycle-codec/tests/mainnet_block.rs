//! Decode a real mainnet block fixture end-to-end.
//!
//! The fixture in `tests/fixtures/mainnet-head.bin` is captured by
//! running `lightcycle inspect --dump-to <path>` against a live
//! java-tron node (committed once and kept stable; refresh by
//! re-running the inspect command and replacing the file). This test
//! is the difference between "codec works on synthetic input" and
//! "codec works on bytes-in-the-wild."
//!
//! IMPORTANT (witness selection): TRON mainnet has dual-engine SR
//! signing — roughly 7 of the 27 active SRs use SM2 (the GM/T 0003
//! Chinese national curve + SM3 hash), the rest use ECKey
//! (secp256k1 + sha256). The current fixture is from an ECKey-class
//! witness so that the sigverify tests below exercise the k256
//! recovery path. When refreshing the fixture, re-run inspect until
//! the captured block is from a known ECKey-class witness; otherwise
//! the sigverify tests will fail with `WitnessAddressMismatch`.
//! SM2 sigverify is a follow-up (separate crate dep, separate path).

use lightcycle_codec::{
    decode_block, recover_witness_address, verify_witness_signature, ContractKind,
};
use lightcycle_types::SrSet;

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

/// java-tron's `blockID` is height-prefixed: the first 8 bytes are
/// the height as big-endian i64, the remaining 24 bytes are the
/// trailing 24 bytes of `sha256(raw_data)`. Wire `parent_hash` uses
/// that height-prefixed form, so the relayer can only chain blocks
/// (`block_id` of N == `parent_id` of N+1) if our `block_id` matches
/// the convention. This test pins the convention against the live
/// fixture's parent_id, since parent_id is whatever the upstream
/// node serialized — the source of truth.
#[test]
fn mainnet_block_id_matches_height_prefixed_convention() {
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).unwrap();
    let height_be = (decoded.header.height as i64).to_be_bytes();
    assert_eq!(
        &decoded.header.block_id.0[..8],
        &height_be,
        "block_id first 8 bytes ({}) don't match height ({}) BE-encoded ({}) — \
         block_id is using bare sha256 instead of TRON's height-prefixed form. \
         Will break chaining: block_id_of_N != parent_id_of_N+1.",
        hex::encode(&decoded.header.block_id.0[..8]),
        decoded.header.height,
        hex::encode(height_be),
    );
    // parent_id is from the wire — check it also follows the convention
    // (its first 8 bytes encode height N-1).
    let parent_height_be = ((decoded.header.height - 1) as i64).to_be_bytes();
    assert_eq!(
        &decoded.header.parent_id.0[..8],
        &parent_height_be,
        "parent_id first 8 bytes don't encode height N-1 — \
         either upstream changed the convention or fixture is wrong",
    );
}

/// The strongest test in this file: a real mainnet block was signed by
/// the witness whose address is in its own header. ECDSA recovery is
/// deterministic, so if our recovery + address-derivation pipeline is
/// even slightly wrong (wrong hash, wrong v normalization, wrong
/// keccak input slice, wrong network prefix) the recovered address
/// won't match. Anything that compiles past here is structurally
/// correct end-to-end.
#[test]
fn mainnet_block_signature_recovers_to_header_witness() {
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).expect("decode");
    let recovered = recover_witness_address(
        &decoded.header.raw_data_hash,
        &decoded.header.witness_signature,
    )
    .expect("recover");
    assert_eq!(
        recovered, decoded.header.witness,
        "recovered address {} doesn't match header witness {}",
        hex::encode(recovered.0),
        hex::encode(decoded.header.witness.0),
    );
}

/// Full verify path against a one-element SR set seeded from the
/// header's own witness. This exercises the membership check in
/// addition to recovery + address match.
#[test]
fn mainnet_block_verify_against_self_seeded_sr_set() {
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).unwrap();
    let sr_set = SrSet::new([decoded.header.witness]);
    verify_witness_signature(&decoded.header, &sr_set).expect("verify");
}

/// Negative control: with an empty SR set, verification must reject
/// even a perfectly-valid signature. Catches "we silently accepted
/// because nobody passed an SR set" regressions.
#[test]
fn mainnet_block_verify_rejects_empty_sr_set() {
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).unwrap();
    let err = verify_witness_signature(&decoded.header, &SrSet::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not in the active SR set"),
        "unexpected error: {msg}"
    );
}
