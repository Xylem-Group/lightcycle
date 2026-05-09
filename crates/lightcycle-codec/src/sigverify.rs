//! Witness signature verification — secp256k1 ECDSA recovery against
//! the active SR set.
//!
//! TRON signs each block with the producer SR's secp256k1 private key.
//! The signature lives at `BlockHeader.witness_signature` and covers
//! `sha256(BlockHeader.raw_data)` — which is the `raw_data_hash` field
//! `decode_header` records alongside the height-prefixed `block_id`.
//! Sigverify reuses that hash; no re-hashing of the raw header.
//!
//! The signature blob is 65 bytes laid out as `r:32 || s:32 || v:1`,
//! where `v` is the recovery id. TRON accepts any of `{0, 1, 27, 28}`
//! for `v` (java-tron's `ECKey.signatureToKey` normalizes both Ethereum-
//! style "v + 27" and raw "v" encodings). We do the same normalization
//! before handing to k256.
//!
//! TRON address derivation from a recovered pubkey:
//!   1. Take the secp256k1 pubkey in SEC1 uncompressed form: `0x04 || x:32 || y:32`.
//!   2. Drop the leading 0x04 prefix to get the 64-byte `(x || y)` form.
//!   3. `keccak256` the 64 bytes; take the last 20.
//!   4. Prepend `0x41` (TRON mainnet network prefix) to get the 21-byte address.
//!
//! Verification fails if any of:
//!   - signature is the wrong length (not 65 bytes),
//!   - k256 rejects the (sig, prehash, recovery_id) combo,
//!   - the recovered address doesn't match the header's `witness_address`,
//!   - the witness isn't in the active SR set.
//!
//! All four are surfaced as distinct `CodecError` variants because the
//! relayer treats them differently — wrong-length is a bad-frame
//! (peer-quality) signal, the rest are protocol/authority violations.

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use sha3::{Digest, Keccak256};

use lightcycle_types::{Address, SrSet};

use crate::block::DecodedHeader;
use crate::error::{CodecError, Result};

/// TRON network prefix on mainnet addresses. Bytes 0..1 of every
/// 21-byte address are this constant.
const TRON_ADDRESS_PREFIX: u8 = 0x41;

/// Length of TRON's witness signature: r:32 || s:32 || v:1.
const WITNESS_SIG_LEN: usize = 65;

/// Verify the witness signature on a decoded block header.
///
/// Performs the full chain of checks:
///   1. Recover the signer's secp256k1 verifying key from the witness
///      signature and the block's `sha256(raw_data)` digest.
///   2. Derive the TRON address from that key.
///   3. Confirm the recovered address matches `header.witness`.
///   4. Confirm that address is in the active SR set.
///
/// Steps 3 and 4 catch different failures: 3 catches "the signature
/// doesn't belong to this header at all" (corruption, replay attempt);
/// 4 catches "the signature is internally consistent but came from an
/// unauthorized signer" (rogue witness, stale SR set on either side).
/// Both are required.
pub fn verify_witness_signature(header: &DecodedHeader, sr_set: &SrSet) -> Result<()> {
    let recovered = recover_witness_address(&header.raw_data_hash, &header.witness_signature)?;

    if recovered != header.witness {
        return Err(CodecError::WitnessAddressMismatch {
            expected: header.witness,
            recovered,
        });
    }

    if !sr_set.contains(&recovered) {
        return Err(CodecError::WitnessNotInSrSet {
            witness: recovered,
            set_size: sr_set.len(),
        });
    }

    Ok(())
}

/// Recover the TRON address that signed `msg_hash`. Returns the 21-byte
/// address; does NOT check that address against any authority list.
///
/// Split out from `verify_witness_signature` because:
///   - benches want to measure recovery in isolation (the cryptographic
///     work; SR-set membership is a single hash lookup),
///   - replay tooling and P2P preflight have a hash + sig but no
///     decoded header to pass.
pub fn recover_witness_address(msg_hash: &[u8; 32], sig_bytes: &[u8]) -> Result<Address> {
    if sig_bytes.len() != WITNESS_SIG_LEN {
        return Err(CodecError::BadSignature {
            field: "witness_signature",
            got: sig_bytes.len(),
        });
    }

    let sig = Signature::from_slice(&sig_bytes[..64])
        .map_err(|_| CodecError::SignatureRecoveryFailed)?;

    // v normalization. TRON's ECKey.signatureToKey accepts both raw
    // recovery ids (0, 1) and Ethereum-style v+27 (27, 28). Anything
    // else is malformed; let RecoveryId::from_byte reject.
    let v = sig_bytes[64];
    let recovery_byte = if v >= 27 { v - 27 } else { v };
    let recovery_id =
        RecoveryId::from_byte(recovery_byte).ok_or(CodecError::SignatureRecoveryFailed)?;

    let verifying_key = VerifyingKey::recover_from_prehash(msg_hash, &sig, recovery_id)
        .map_err(|_| CodecError::SignatureRecoveryFailed)?;

    Ok(verifying_key_to_tron_address(&verifying_key))
}

/// Derive a TRON address from a secp256k1 verifying key.
///
/// `0x41 || keccak256(uncompressed_xy_64bytes)[12..]`. The 0x04 SEC1
/// prefix on the uncompressed point is NOT included in the keccak
/// input — TRON's reference implementation skips it explicitly.
fn verifying_key_to_tron_address(vk: &VerifyingKey) -> Address {
    let encoded = vk.to_encoded_point(false);
    // SEC1 uncompressed: 0x04 || x:32 || y:32 (65 bytes total). Skip
    // the leading 0x04.
    let xy = &encoded.as_bytes()[1..];
    debug_assert_eq!(xy.len(), 64);

    let hash = Keccak256::digest(xy);

    let mut addr = [0u8; 21];
    addr[0] = TRON_ADDRESS_PREFIX;
    addr[1..].copy_from_slice(&hash[12..]);
    Address(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
    use lightcycle_types::BlockId;

    /// Sign a message hash with a fresh random key, returning
    /// `(sig_with_v, recovered_address)`. Used to drive the verify path
    /// end-to-end without a network.
    fn sign_test(
        msg_hash: &[u8; 32],
    ) -> ([u8; WITNESS_SIG_LEN], Address) {
        // Fixed seed so test failures are reproducible. Derived key is
        // just a valid secp256k1 scalar; nothing about it has to match
        // any real-network witness.
        let signing_key = SigningKey::from_bytes(&[0x42; 32].into()).expect("fixed-seed key");
        let verifying_key = *signing_key.verifying_key();

        // PrehashSigner gives us deterministic ECDSA + recovery id over
        // an already-computed hash.
        let (sig, recovery_id): (Signature, RecoveryId) =
            signing_key.sign_prehash(msg_hash).expect("sign");

        let mut out = [0u8; WITNESS_SIG_LEN];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = recovery_id.to_byte();

        (out, verifying_key_to_tron_address(&verifying_key))
    }

    fn dummy_header(witness: Address, sig: Vec<u8>) -> DecodedHeader {
        DecodedHeader {
            height: 82_524_196,
            block_id: BlockId([0xaa; 32]),
            parent_id: BlockId([0xbb; 32]),
            raw_data_hash: [0xaa; 32],
            tx_trie_root: [0xcc; 32],
            timestamp_ms: 1_777_854_558_000,
            witness,
            witness_signature: sig,
            version: 34,
        }
    }

    #[test]
    fn recovers_address_we_signed_with() {
        let msg = [0xaa; 32];
        let (sig, addr) = sign_test(&msg);
        let recovered = recover_witness_address(&msg, &sig).expect("recover");
        assert_eq!(recovered, addr);
    }

    #[test]
    fn rejects_wrong_length_signature() {
        let err = recover_witness_address(&[0u8; 32], &[0u8; 64]).unwrap_err();
        assert!(matches!(err, CodecError::BadSignature { got: 64, .. }));

        let err = recover_witness_address(&[0u8; 32], &[0u8; 66]).unwrap_err();
        assert!(matches!(err, CodecError::BadSignature { got: 66, .. }));
    }

    #[test]
    fn rejects_invalid_recovery_id() {
        let mut sig = [0u8; 65];
        // Plausible r/s (any 32-byte values < curve order are fine for
        // *parsing*; recovery may still fail, which is what we want here
        // since we want the recovery_id branch to reject first).
        sig[..64].fill(0x11);
        sig[64] = 7; // Not 0/1/27/28.
        let err = recover_witness_address(&[0u8; 32], &sig).unwrap_err();
        assert!(matches!(err, CodecError::SignatureRecoveryFailed));
    }

    #[test]
    fn normalizes_eth_style_v_27() {
        let msg = [0x55; 32];
        let (mut sig, addr) = sign_test(&msg);
        // sign_test emits v in {0,1}. Re-encode as Ethereum's v+27 and
        // confirm we accept it (java-tron parity).
        sig[64] += 27;
        let recovered = recover_witness_address(&msg, &sig).expect("recover with v+27");
        assert_eq!(recovered, addr);
    }

    #[test]
    fn verify_succeeds_when_witness_in_sr_set() {
        let msg = [0xee; 32];
        let (sig, addr) = sign_test(&msg);
        let mut header = dummy_header(addr, sig.to_vec());
        header.raw_data_hash = msg;

        let sr_set = SrSet::new([addr]);
        verify_witness_signature(&header, &sr_set).expect("verify");
    }

    #[test]
    fn verify_rejects_wrong_witness_address() {
        let msg = [0xee; 32];
        let (sig, real_signer) = sign_test(&msg);

        // Header claims a different witness than the one that actually
        // signed. Recovery succeeds but the address doesn't match.
        let bogus_witness = Address([0x41; 21]);
        let mut header = dummy_header(bogus_witness, sig.to_vec());
        header.raw_data_hash = msg;

        let sr_set = SrSet::new([real_signer, bogus_witness]);
        let err = verify_witness_signature(&header, &sr_set).unwrap_err();
        match err {
            CodecError::WitnessAddressMismatch {
                expected,
                recovered,
            } => {
                assert_eq!(expected, bogus_witness);
                assert_eq!(recovered, real_signer);
            }
            other => panic!("expected WitnessAddressMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_signer_not_in_sr_set() {
        let msg = [0xee; 32];
        let (sig, signer) = sign_test(&msg);
        let mut header = dummy_header(signer, sig.to_vec());
        header.raw_data_hash = msg;

        // Empty SR set: signature is internally consistent but signer
        // isn't authorized.
        let sr_set = SrSet::default();
        let err = verify_witness_signature(&header, &sr_set).unwrap_err();
        assert!(matches!(err, CodecError::WitnessNotInSrSet { .. }));
    }
}
