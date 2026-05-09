//! Sigverify microbench. Validates the BENCHMARKS.md B2 hypothesis —
//! "≥5K blk/s sustained with sigverify on" — by isolating the
//! cryptographic cost from everything else.
//!
//! Three measurements:
//!
//!   1. `recover_only` — bare ECDSA recovery + address derivation
//!      against a 32-byte digest, no decode. The lower bound on per-
//!      block sigverify cost; what the tight-loop verifier will pay
//!      once it has a `(prehash, signature)` pair in hand.
//!
//!   2. `decode_then_recover` — full pipeline on a real mainnet block:
//!      protobuf decode + recover witness from header. This is the
//!      number that maps to "blocks per second sustained with
//!      sigverify on" — what the relayer's hot path actually does.
//!
//!   3. `verify_with_sr_set` — same as (2) plus the membership check
//!      against a full 27-member SR set. Sanity check that the
//!      HashSet lookup is negligible cost (it should be ~5 ns).
//!
//! What we deliberately do NOT measure:
//!   - Sigverify against synthetic-key signed blocks. The mainnet
//!     fixture is the realistic test; synthetic keys exercise the
//!     same code paths and would just be a tighter loop.
//!   - SM2 sigverify. Not implemented yet (separate engine, separate
//!     microbench when it lands; see codec docs).
//!   - Throughput of cold-cache tx-list iteration. That belongs to
//!     the decode bench, not sigverify.
//!
//! Run with `cargo bench -p lightcycle-codec --bench sigverify`. To
//! compare against the decode-only baseline, run both benches and
//! diff `target/criterion/`.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use lightcycle_codec::{decode_block, recover_witness_address, verify_witness_signature};
use lightcycle_types::{Address, SrSet};

/// Real mainnet block fixture, captured from an ECKey-class witness so
/// that the recovery path is exercised end-to-end. See the test crate's
/// docstring on why ECKey-class matters (~25% of SRs use SM2 instead;
/// for those, recover_witness_address returns a non-matching address).
const MAINNET_HEAD_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/mainnet-head.bin");

fn bench_recover_only(c: &mut Criterion) {
    // Pre-decode once so the bench measures only the cryptographic
    // recovery, not the protobuf parse. Both prehash and signature are
    // owned by the bench closure; nothing allocates on the hot path.
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).expect("decode");
    let prehash = decoded.header.raw_data_hash; // sha256(raw_data); block_id is height-prefixed
    let sig = decoded.header.witness_signature.clone();

    let mut group = c.benchmark_group("sigverify");
    group.throughput(Throughput::Elements(1));
    group.bench_function("recover_only", |b| {
        b.iter(|| {
            let addr =
                recover_witness_address(black_box(&prehash), black_box(&sig)).expect("recover");
            black_box(addr);
        });
    });
    group.finish();
}

fn bench_decode_then_recover(c: &mut Criterion) {
    let mut group = c.benchmark_group("sigverify");
    group.throughput(Throughput::Elements(1));
    group.bench_function("decode_then_recover", |b| {
        b.iter(|| {
            let decoded = decode_block(black_box(MAINNET_HEAD_FIXTURE)).expect("decode");
            let addr = recover_witness_address(
                black_box(&decoded.header.raw_data_hash),
                black_box(&decoded.header.witness_signature),
            )
            .expect("recover");
            black_box((decoded, addr));
        });
    });
    group.finish();
}

fn bench_verify_with_sr_set(c: &mut Criterion) {
    // Build a representative 27-member SR set seeded with the actual
    // signer plus 26 distinct decoy addresses so the HashSet has real
    // load. The signer must be present for `verify_witness_signature`
    // to succeed — we want to measure the success path, not the
    // not-in-set short-circuit.
    let decoded = decode_block(MAINNET_HEAD_FIXTURE).expect("decode");
    let signer = decoded.header.witness;
    let decoys: Vec<Address> = (0..26)
        .map(|i| {
            let mut a = [0u8; 21];
            a[0] = 0x41;
            a[1..].copy_from_slice(&[i as u8; 20]);
            Address(a)
        })
        .collect();
    let mut all = decoys;
    all.push(signer);
    let sr_set = SrSet::new(all);
    assert_eq!(sr_set.len(), 27);

    let mut group = c.benchmark_group("sigverify");
    group.throughput(Throughput::Elements(1));
    group.bench_function("verify_with_sr_set", |b| {
        b.iter(|| {
            verify_witness_signature(black_box(&decoded.header), black_box(&sr_set))
                .expect("verify");
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_recover_only,
    bench_decode_then_recover,
    bench_verify_with_sr_set
);
criterion_main!(benches);
