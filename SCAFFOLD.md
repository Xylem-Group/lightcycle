# Scaffold notes

What's in the repo right now and what to do next.

## What's in

- **Workspace skeleton** with 8 crates wired up (`Cargo.toml` workspace + per-crate manifests).
- **`README.md`** with vision, design goals, quickstart, layout.
- **`ARCHITECTURE.md`** with component diagram, dataflow, trust model.
- **`BENCHMARKS.md`** with 10 benchmark scenarios (B1–B10), methodology, isolation hygiene, and reference hardware.
- **CLI binary** (`lightcycle stream | inspect | version`) — argument parsing wired, business logic stubbed.
- **Domain types** in `lightcycle-types`: `BlockHeight`, `BlockId`, `TxHash`, `Address`, `Step`, `Cursor`.
- **Trait surface** for `Source`, `BlockCache`, `ReorgEngine`, codec entry points.
- **Docker stack** (`docker compose up` brings `java-tron` + `lightcycle` + Prometheus + Grafana).
- **CI** (fmt, clippy, test, cargo-deny, doc).
- **`scripts/sync-protos.sh`** to vendor protobuf from `java-tron` and `firehose-core` upstreams.

## What's NOT in (intentional, next steps)

In rough priority order:

1. **Vendor the protobuf**. Run `scripts/sync-protos.sh`, pin commit SHAs in `proto/README.md`, fill in `crates/lightcycle-proto/build.rs`, and confirm `cargo build` regenerates types.
2. **`lightcycle-codec::decode_block`**. Take raw protobuf, return a `DecodedBlock`. This is the hot path; benchmark with `criterion` (B3) early.
3. **`lightcycle-source::p2p`**. Speak the TRON P2P handshake, request block ranges, follow head. Reference: `java-tron`'s `org.tron.core.net.peer` package.
4. **`lightcycle-source::rpc`**. Polling loop against `/wallet/getnowblock`, `/wallet/getblockbynum`. Use this to bootstrap before P2P is ready.
5. **`ReorgEngine`**. Maintain last ~30 unsolidified blocks; emit `UNDO`/`NEW` on canonical chain switch; mark `IRREVERSIBLE` at the solidified head.
6. **`lightcycle-firehose::serve`**. Wire the `sf.firehose.v2.Stream` gRPC service to the relayer's outgoing channel.
7. **Substreams smoke test**. Point `substreams run` at `localhost:13042`, confirm a trivial Rust module compiles and tails blocks end-to-end.
8. **B1, B2, B5 benchmarks**. Get these running first; they're the headline numbers.

## Dependency hygiene

- `cargo-deny` is wired in CI. Run locally with `cargo deny check`.
- All deps go through `[workspace.dependencies]`; per-crate `Cargo.toml` references with `.workspace = true`. This prevents version drift.
- Deliberate choices to avoid bloat:
  - `k256` (pure Rust) over `secp256k1` (libsecp wrapper) — avoids C build deps.
  - `redb` over `rocksdb` — pure Rust, no LLVM at build time, fewer surprises.
  - `reqwest` with rustls (not openssl) — fewer system deps, easier to statically link.
  - We do **not** pull in `alloy` wholesale; only the primitives we need (`primitive-types`).
  - We do **not** pull in `revm` yet; will only add when/if we need TVM execution simulation.

## What to commit first

Recommended initial commit sequence (fits in 1 PR each):

1. `init: workspace skeleton + docs` — what's here now.
2. `proto: vendor java-tron and firehose-core schemas` — `scripts/sync-protos.sh` output.
3. `codec: block decode + signature verification` — first real logic, plus criterion bench.
4. `source: rpc fallback` — easier than P2P, gets us end-to-end first.
5. `relayer: reorg engine + cursor` — the correctness-critical piece.
6. `firehose: gRPC server` — the public API surface.
7. `bench: B1 + B2 + B5 harness` — get the numbers we promised.
