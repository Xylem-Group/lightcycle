//! B3 microbench (per BENCHMARKS.md): protobuf decode + transaction
//! enumeration on a single core, isolated from I/O.
//!
//! What we measure: how long `decode_block` takes for blocks of
//! representative shape — empty, light, and busy. This isolates the
//! protobuf parsing + transaction iteration from any sigverify, ABI
//! decode, or storage cost. Sigverify is a separate bench when we
//! land it; B3 is the lower bound.
//!
//! What we deliberately do NOT measure here:
//!   - Real-network blocks (those land in the harness, B2)
//!   - Witness sigverify (separate microbench when implemented)
//!   - Event ABI decode (same — separate concern)
//!   - Allocation strategy / arena / zero-copy paths (latency only,
//!     not allocator throughput; cargo profiling is the right tool
//!     for that)
//!
//! Run with `cargo bench -p lightcycle-codec`. Results land in
//! `target/criterion/` and an aggregate `cargo bench` view; copy
//! them into `benches/results/<date>/<tag>/` when committing.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use lightcycle_codec::decode_block;
use prost::Message;

use lightcycle_proto::tron::protocol::{
    block_header::Raw as RawHeader,
    transaction::{contract::ContractType, Contract, Raw as RawTx},
    Block, BlockHeader, Transaction,
};

/// Build a syntactically valid encoded block at a given height with
/// `n_txs` trivial Transfer transactions. Mirrors the helper in the
/// codec's unit tests; duplicated here rather than re-exported so the
/// codec crate doesn't need to expose a `pub fn synth_block` for
/// benches' sake.
fn synth_block_bytes(height: i64, n_txs: usize) -> Vec<u8> {
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

    Block {
        transactions: txs,
        block_header: Some(header),
    }
    .encode_to_vec()
}

fn bench_decode(c: &mut Criterion) {
    // Three shapes:
    //   - 0 txs:  empty block (rare on mainnet but the lower bound)
    //   - 50 txs: a quiet mainnet block
    //   - 500 txs: a busy mainnet block at peak (USDT-heavy windows)
    //
    // criterion will iterate each independently and report ns/iter.
    // We also set throughput so the report shows blocks/sec — the
    // headline number BENCHMARKS.md wants.
    for &n in &[0_usize, 50, 500] {
        let bytes = synth_block_bytes(82_500_000, n);
        let mut group = c.benchmark_group("decode_block");
        group.throughput(Throughput::Elements(1));
        group.bench_function(format!("{n}_tx"), |b| {
            b.iter(|| {
                let decoded = decode_block(black_box(&bytes)).expect("decode");
                black_box(decoded);
            });
        });
        group.finish();
    }
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
