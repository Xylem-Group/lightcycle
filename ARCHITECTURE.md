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

- **Block header verification**: secp256k1 signature recovery, check the recovered witness address against the active SR set, validate the txTrieRoot. **On the dual-engine question**: java-tron has an `SM2` codepath (China's GM/T 0003 national standard) alongside `ECKey`/secp256k1, and from a distance one might assume both are in use. They are not. Investigation 2026-05-09 (java-tron #6588, PR #6627, `MiscConfig.java`): `isECKeyCryptoEngine` is hard-true on mainnet, no mainnet SR signs with SM2, and core devs have proposed deleting the unused module. lightcycle therefore implements only secp256k1 sigverify, and the codec's `WitnessAddressMismatch` branch represents a real bug or an attacker — not a benign engine mismatch.
- **Transaction decoding**: TRON has 40+ contract types. The four high-volume ones (`Transfer`, `TransferAsset`/TRC-10, `TriggerSmartContract` — TRC-20/DEX/USDT all flow through here, `CreateSmartContract`) get fully-decoded payload variants on `DecodedContract`; everything else lands in `Other { kind, raw }` with the original `Any.value` bytes preserved so consumers can decode against the matching java-tron protobuf message themselves.
- **TransactionInfo decoding**: the side channel java-tron exposes via `GetTransactionInfoByBlockNum` / `GetTransactionInfoById`. The block proto carries the request (signed tx); `TransactionInfo` carries the result (success/fail, energy/bandwidth, emitted logs, internal sub-calls). We surface logs raw, internal txs as `InternalTx`, and the energy/bandwidth breakdown as `ResourceCost` — consumers reconstructing token flow, computing TRX cost, or replaying internal calls don't need a second round-trip per tx.
- **Event log decoding**: two complementary surfaces. The universal token events (`TRC-20 Transfer`/`Approval`, `TRC-721 Transfer`) ship in `events.rs` and are recognized by topic-0 hash + topic-count alone — every standard token contract decodes without any setup. Arbitrary contract events go through `abi.rs`: operators register Solidity-style signatures (e.g. `"Swap(address indexed sender, uint256 amount0In, uint256 amount1In, uint256 amount0Out, uint256 amount1Out, address indexed to)"`); the registry computes `topic[0] = keccak256(canonical_signature)` and decodes matching logs into a structured `DecodedEvent` with positional + named param access. v0.1 supports the static-type subset (`address`, `bool`, `uintN`/`intN`, `bytesN`); dynamic types (`string`, `bytes`, `T[]`) and tuples surface a typed `AbiParseError::UnsupportedType` so consumers know the boundary.
- **Address ergonomics**: `Address::to_base58check` / `from_base58check` for the human-facing `T...` form; raw 21-byte forms remain the wire/storage canonical.

### lightcycle-relayer

The orchestrator. Owns the canonical head pointer and the live block buffer.

- **Reorg engine**: maintains the last N unsolidified blocks (`N ≈ 30` covers TRON's longest realistic reorg). When the canonical head changes, emits `UNDO` for orphaned blocks before `NEW` for the new branch. Consumers can build correct materialized state without reasoning about TRON's solidification rules themselves.
- **Finality tagging**: chain-driven. Per ADR-0021 (`alexandria/docs/adr/0021-consistency-horizons-and-the-distributed-verification-floor.md`), the engine reads the chain's solidified head from `WalletSolidity.GetNowBlock` every tick and emits `IRREVERSIBLE` only when a buffered block crosses that height. We do NOT compute finality from a confirmation count — the chain has its own SR-supermajority finality machine and we lean on it.
- **Fork ledger**: on reorg, the engine emits an internal `ForkObserved` ledger entry naming the kept tip + the orphaned tips. When the chain's solidified head later advances past the observation, a corresponding `ForkResolved` entry records the chain's resolution. Together they form the audit ledger ADR-0021 §3 mandates: the engine never *decides* which fork wins, it records the chain's claim. Ledger entries flow through `tracing` logs + the `lightcycle_relay_outputs_total{step="fork_observed|fork_resolved"}` counter; they have no Firehose v2 wire shape (the v2 schema has no STATUS variant) so they don't ship over `Stream.Blocks`.
- **Cursor management**: every emission carries an opaque cursor encoding `{height, blockId, forkId}`. Resumption is deterministic and reorg-correct.

### lightcycle-firehose

gRPC server speaking the [`sf.firehose.v2`](https://github.com/streamingfast/proto) protocol used by Substreams. Multiplexes one upstream block stream to many downstream consumers via a `tokio::sync::broadcast` hub — slow subscribers get `RESOURCE_EXHAUSTED` rather than back-pressuring the engine, which is correct semantics for a relayer.

Three Firehose v2 services ship. **`Stream.Blocks`** runs live-tail by default; when an in-memory block cache is attached (the CLI wires one when `--firehose-listen` is set), requests with `start_block_num` or `cursor` walk the cache from the requested height to its tip and then transition into the live broadcast (with dedup so a block emitted from the cache isn't re-emitted from live). When a persistent block archive is also attached (`--archive-path`), backfill walks the archive first for any heights below the cache window, then chains into the cache walk — extending resume capability past the in-memory horizon. `stop_block_num`, `final_blocks_only`, and `transforms` are rejected with `FailedPrecondition`. **`Fetch.Block`** does point-in-time block lookup by `BlockNumber` reference; the service delegates to a `BlockOracle`. The CLI composes a `CachingBlockOracle` (read-through over the same in-memory cache the relayer feeds) wrapping a `GrpcBlockOracle` (dedicated `GrpcSource` connection — separate from the relayer's source so request-driven fetches don't serialize behind the live tail); the archive (when configured) is checked first to short-circuit upstream RPC for any height past the cache window. `BlockHashAndNumber` and `Cursor` references plus `transforms` are rejected with `FailedPrecondition`. **`EndpointInfo.Info`** reports chain identity for orchestrator sanity-check.

`Response.metadata` is fully populated (num, id, parent_num, parent_id, lib_num=0 for now, time). `Response.block` carries a `google.protobuf.Any` whose `type_url` is `type.googleapis.com/sf.tron.type.v1.Block` and whose value is the prost-encoded `sf.tron.type.v1.Block` — header, transactions, and contract payloads (typed for the four high-volume contract kinds: `Transfer`, `TransferAsset`, `TriggerSmartContract`, `CreateSmartContract`; raw bytes plus wire kind tag for everything else, so consumers can decode locally against java-tron's protobuf for the long-tail governance/admin contracts).

`Transaction.info` is populated from java-tron's `getTransactionInfoByBlockNum` side channel: per-tx logs, internal transactions, resource accounting, and success/fee. Default-on; CLI exposes `--fetch-tx-info=false` to halve the per-tick RPC cost when consumers don't read it. RPC failures soft-degrade (block ships with empty `tx_infos`; logged + counted as `lightcycle_relay_tx_info_fetch_total{result="rpc_error"}`) rather than halting the stream. Encoder joins by tx hash so wire ordering doesn't matter.

`Block.finality` carries the chain-reported finality envelope per ADR-0021 §1: a `tier` enum (`SEEN | CONFIRMED | FINALIZED`) plus the `solidified_head_number` the tier was derived against. Consumers that need only "safe to use as a cross-replica truth" filter on `tier == FINALIZED`. The Firehose v2 `Response.metadata.lib_num` is sourced from the same solidified-head value, so dashboards and orchestrators see the chain's claim, not a count we computed.

The proto schema lives at `proto/sf/tron/type/v1/block.proto` and is compiled into `lightcycle_proto::sf::tron::type_v1`. The encoder (`lightcycle_firehose::encode_block`) maps `BufferedBlock` + `BlockFinality` → wire proto in pure CPU.

### lightcycle-store

Local persistence + the consistency-horizon SLO. Per ADR-0021, this crate is constitutionally bound to chain-finality as the only legal cross-replica consistency source — no Raft / Paxos / custom-quorum shims (the chain's SR consensus already solves the verification problem and the Das Sarma round-complexity floor makes any locally-engineered alternative provably worse).

- **Consistency-horizon SLO** (landed): `ConsistencyHorizonObserver` records `seen_at` per block id when the relayer first surfaces a `tier=Seen` block, then closes the loop on the `tier=Finalized` transition by observing the elapsed wall-clock time into `lightcycle_store_block_seen_to_finalized_seconds`. **Target: p99 ≤ 5s under healthy operation; alert if >5s sustained for >5 min.** Exported via the standard prometheus exporter (`lightcycle relay --metrics-listen ...`).
- **Block cache** (landed): bounded in-memory `BlockCache<T>` indexed by both height and block id, generic over `T` so the crate stays free of `lightcycle-relayer` deps. The relayer writes on every `Output::New`/`Output::Undo`; firehose `Fetch.Block` and `Stream.Blocks` backfill read from it. Eviction is "drop the lowest-height entry first" — height-LRU is the right policy because consumer demand drops sharply with block age.
- **Block archive** (landed): redb-backed `BlockArchive` that catches blocks past the chain's solidified-head threshold. Same opaque-bytes API shape as the cursor store; the firehose layer encodes `pb::Block` bytes into it via the archiver task. Stream.Blocks backfill walks archive → cache → live; Fetch.Block hits the archive on cache miss. Operator-facing CLI: `--archive-path` + `--archive-retention-blocks`. Append-only on the happy path (only past-finality blocks land here, by design).
- **Cursor store** (landed): per-consumer cursor checkpoints in a redb-backed map. Per ADR-0021 explicitly **not** a cross-replica primitive — each replica tracks the consumers attached to it.
- **SR set checkpoints** (planned): trusted starting point + maintenance-period diffs, so cold restarts don't have to re-derive the whole history.

Future cross-replica work must implement `lightcycle_store::ConsistencySource`. The blessed implementation is `FinalityFromChain` (snapshots the relayer's view of the chain's solidified head). Reviewers proposing alternatives should be redirected to ADR-0021.

## Finality discipline (ADR-0021)

`lightcycle` treats the TRON chain's own SR-supermajority finality machine as the consistency source for any global question — "what's the head?", "are these two replicas in agreement?", "which fork is canonical?" — and never recomputes it. This is the core architectural posture of ADR-0021 (`alexandria/docs/adr/0021-consistency-horizons-and-the-distributed-verification-floor.md`), which itself follows from the Das Sarma (2012) round-complexity floor on distributed verification: any locally-engineered cross-replica agreement protocol has a provable lower bound that no engineering optimization removes, while the chain already provides a finality machine for free.

Three load-bearing rules implement this discipline.

1. **Finality is read, never computed.** Every tick, the relayer fetches `WalletSolidity.GetNowBlock` and pushes the solidified-head height into the reorg engine via `set_solidified_head`. `Output::Irreversible` fires only when a buffered block crosses that head. There is no confirmation-count threshold; a hard-coded "19 of 27" rule would be a Das Sarma-style verification claim we made ourselves rather than reading from the chain.
2. **Reorgs are observed, not decided.** When the source surfaces a competing tip, the engine performs an optimistic switch (Undo of orphans, New of the new tip) and emits a `ForkObserved` ledger entry naming both. The engine does not "decide" which fork is canonical — when the chain's solidified head later advances past the observation, a `ForkResolved` entry records the chain's resolution. The two together constitute the structured ledger of fork events that ADR-0021 §3 mandates.
3. **`tier=Seen` ≠ exists.** A block tagged Seen is a candidate for finalization, not a committed fact. Only `tier=Finalized` is safe to use as a cross-replica or cross-region truth. Future `lightcycle-store` cross-replica work must call into `ConsistencySource` (the only blessed implementation is `FinalityFromChain`), not invent a new agreement protocol.

The consistency-horizon SLO `lightcycle_store_block_seen_to_finalized_seconds` measures the practical realization of this discipline — the wall-clock latency from a block being first observed to being finalized — and gates whether the chain-finality oracle is healthy. Target p99 ≤ 5s; alert rules in `docker/prometheus-alerts.yml` fire `ConsistencyHorizonP99Breached` on a 5-minute sustained breach, plus three companion rules (`SolidifiedHeadFetchFailing`, `SolidifiedHeadStalled`, `ConsistencyHorizonEvictionsHigh`) on the load-bearing causes so operators can triangulate without paging twice.

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
