# Architecture

`lightcycle` is a streaming relayer: bytes in from TRON full nodes, structured events out via gRPC. It is intentionally not a validating client, not a full node, and not a state-execution engine. It is a fast pipeline with strong correctness guarantees.

## Dataflow

```
   ┌──────────────────────┐
   │  TRON full node      │   (java-tron, ours or remote)
   │  P2P :18888          │
   │  HTTP :8090          │
   └──────────┬───────────┘
              │  raw protobuf blocks
              ▼
   ┌──────────────────────────────┐
   │  lightcycle-source           │
   │   • P2P client (preferred)   │
   │   • RPC fallback             │
   │   • SR set tracker           │
   └──────────┬───────────────────┘
              │  Block (raw bytes + height)
              ▼
   ┌──────────────────────────────┐
   │  lightcycle-codec            │
   │   • protobuf decode          │
   │   • secp256k1 sig recovery   │
   │   • TRC-10/20/721 event decode │
   │   • internal tx extraction   │
   │   • resource accounting      │
   └──────────┬───────────────────┘
              │  Block (decoded)
              ▼
   ┌──────────────────────────────┐
   │  lightcycle-relayer          │
   │   • reorg engine             │
   │   • finality tagging         │
   │   • cursor management        │
   └──────────┬───────────────────┘
              │  StreamableBlock (NEW/UNDO/IRREVERSIBLE)
              ▼
   ┌──────────────────────────────┐
   │  lightcycle-firehose         │
   │   • bstream.v2 gRPC server   │
   │   • multiplexed consumers    │
   │   • per-cursor backpressure  │
   └──────────────────────────────┘
                                │
                                ▼
                        Substreams / Kafka / sigflow / …
```

## Components

### lightcycle-source

Pulls raw blocks. Two modes:

- **P2P mode** (preferred). Speaks TRON's protobuf protocol over TCP. Connects to one or more `java-tron` peers, requests block ranges, follows the head. Lower latency, fewer round trips, no rate limits.
- **RPC mode** (fallback). Polls `java-tron`'s HTTP/gRPC endpoint. Higher latency but operationally trivial. Good for backfill against TronGrid or a private full node.

Maintains the **active SR set** by tracking maintenance period transitions (every 7,200 blocks ≈ 6 hours). The SR set is the source of truth for which witness signatures count as valid; we re-derive it from on-chain vote events rather than trusting the peer's view.

### lightcycle-codec

Pure CPU, no I/O. Takes raw protobuf bytes, returns structured types.

- **Block header verification**: secp256k1 signature recovery, check the recovered witness address against the active SR set, validate the txTrieRoot. **Caveat — TRON has dual-engine SRs.** Empirically, ~25% of mainnet active witnesses sign blocks with SM2 (China's GM/T 0003 national standard, on a different curve from secp256k1, with SM3 hashing), the rest with ECKey/secp256k1+sha256. The chain accepts both via java-tron's `SignUtils` engine selection. v0.1 implements only the ECKey path; SM2 is a follow-up because it pulls in a separate crypto stack and is irrelevant for the ECKey-class blocks that produce the bulk of throughput. SM2-class blocks pass-through verification still works for indexing — `verify_witness_signature` returns `WitnessAddressMismatch` and the relayer can downgrade to "trust the peer" mode for those headers without losing any tx data.
- **Transaction decoding**: TRON has 40+ contract types. The four high-volume ones (`Transfer`, `TransferAsset`/TRC-10, `TriggerSmartContract` — TRC-20/DEX/USDT all flow through here, `CreateSmartContract`) get fully-decoded payload variants on `DecodedContract`; everything else lands in `Other { kind, raw }` with the original `Any.value` bytes preserved so consumers can decode against the matching java-tron protobuf message themselves.
- **TransactionInfo decoding**: the side channel java-tron exposes via `GetTransactionInfoByBlockNum` / `GetTransactionInfoById`. The block proto carries the request (signed tx); `TransactionInfo` carries the result (success/fail, energy/bandwidth, emitted logs, internal sub-calls). We surface logs raw, internal txs as `InternalTx`, and the energy/bandwidth breakdown as `ResourceCost` — consumers reconstructing token flow, computing TRX cost, or replaying internal calls don't need a second round-trip per tx.
- **Event log decoding**: the universal token events (`TRC-20 Transfer`/`Approval`, `TRC-721 Transfer`) are recognized by topic-0 hash + topic-count and ship in v0.1 — they cover every standard token contract without any registry. Custom contract events (governance, oracles, anything not following the standard signatures) require an ABI registry and land behind a future `decode_event(log, abi)` entry point. ABI registry shape is pluggable: file-based, HTTP, or on-chain via `getContract`.
- **Address ergonomics**: `Address::to_base58check` / `from_base58check` for the human-facing `T...` form; raw 21-byte forms remain the wire/storage canonical.

### lightcycle-relayer

The orchestrator. Owns the canonical head pointer and the live block buffer.

- **Reorg engine**: maintains the last N unsolidified blocks (`N ≈ 30` covers TRON's longest realistic reorg). When the canonical head changes, emits `UNDO` for orphaned blocks before `NEW` for the new branch. Consumers can build correct materialized state without reasoning about TRON's solidification rules themselves.
- **Finality tagging**: marks blocks `IRREVERSIBLE` once they cross the solidified head (~19 of 27 confirmations, ~57 seconds).
- **Cursor management**: every emission carries an opaque cursor encoding `{height, blockId, forkId}`. Resumption is deterministic and reorg-correct.

### lightcycle-firehose

gRPC server speaking the [`sf.firehose.v2`](https://github.com/streamingfast/proto) protocol used by Substreams. Multiplexes one upstream block stream to many downstream consumers via a `tokio::sync::broadcast` hub — slow subscribers get `RESOURCE_EXHAUSTED` rather than back-pressuring the engine, which is correct semantics for a relayer.

v0.1 ships **live mode only**: `Stream.Blocks` rejects requests carrying `cursor`, `start_block_num`, `stop_block_num`, `final_blocks_only`, or `transforms` with `FailedPrecondition`. Backfill / replay and the `Fetch.Block` service land when [`lightcycle-store`] is wired into the pipeline (cursor → buffered/persisted block lookup). `EndpointInfo.Info` is implemented and reports chain identity for orchestrator sanity-check.

`Response.metadata` is fully populated (num, id, parent_num, parent_id, lib_num=0 for now, time). `Response.block` carries a `google.protobuf.Any` whose `type_url` is `type.googleapis.com/sf.tron.type.v1.Block` and whose value is the prost-encoded `sf.tron.type.v1.Block` — header, transactions, and contract payloads (typed for the four high-volume contract kinds: `Transfer`, `TransferAsset`, `TriggerSmartContract`, `CreateSmartContract`; raw bytes plus wire kind tag for everything else, so consumers can decode locally against java-tron's protobuf for the long-tail governance/admin contracts).

The proto carries a `Transaction.info` field for `TransactionInfo` (logs, internal txs, resource accounting), but it's left unset in v0.1 — the ingest pipeline doesn't yet fetch java-tron's `getTransactionInfoByBlockNum` side channel. When that lands, existing consumers transparently start seeing populated `info` without any wire change. This is the dominant TRC-20 indexing use case; consumers that need event logs today must continue calling the upstream node's tx-info endpoint themselves.

The proto schema lives at `proto/sf/tron/type/v1/block.proto` and is compiled into `lightcycle_proto::sf::tron::type_v1`. The encoder (`lightcycle_firehose::encode_block`) maps `BufferedBlock` → wire proto in pure CPU.

### lightcycle-store

Local persistence:

- **Block cache**: recent N blocks for reorg replay, in-memory with spill to `redb`.
- **Cursor store**: per-consumer cursor checkpoints (optional, mostly for ops dashboards).
- **SR set checkpoints**: trusted starting point + maintenance-period diffs, so cold restarts don't have to re-derive the whole history.

## Trust model

Default: trust the configured `java-tron` peers to deliver canonical data, but verify witness signatures against an independently maintained SR set. This catches a peer serving forged headers, while keeping ops simple.

Optional **dual-source verification**: cross-check block hashes across two independent peers, alert on divergence. Cheap defense against a single compromised peer.

A **fully trustless** mode (sync the SR set from genesis, validate every signature, refuse data from peers that cannot prove inclusion) is on the roadmap but not the v0.1 target. The juice is not worth the squeeze for most indexer use cases — the dominant attack surface is the consumer's own pipeline, not the relayer.

## What `lightcycle` does NOT do

- Full state reconstruction. We index events and transactions, not state. State queries hit the underlying `java-tron` directly.
- Proof generation. We can verify proofs we receive; we don't generate them.
- Block production / consensus participation. This is a relayer, not a validator.
- Rewriting `java-tron`. We need a full node somewhere; we just don't run it ourselves.

## Module dependency graph

```
                     ┌─────────────┐
                     │    cli      │
                     └──────┬──────┘
                            ▼
                     ┌─────────────┐
                     │   relayer   │
                     └──┬───────┬──┘
                        │       │
              ┌─────────▼┐    ┌─▼──────────┐
              │ firehose │    │   source   │
              └────┬─────┘    └─────┬──────┘
                   │                │
                   └────────┬───────┘
                            ▼
                     ┌─────────────┐
                     │    codec    │
                     └──────┬──────┘
                            ▼
                     ┌─────────────┐
                     │    types    │
                     └──────┬──────┘
                            ▼
                     ┌─────────────┐
                     │    proto    │  (generated)
                     └─────────────┘

                     ┌─────────────┐
                     │    store    │   (used by relayer + firehose)
                     └─────────────┘
```
