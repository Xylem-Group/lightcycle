# lightcycle

> A streaming relayer for the TRON grid.

`lightcycle` is a fast, reorg-aware, [Firehose-protocol-compatible](https://firehose.streamingfast.io/) data source for the TRON blockchain. It ingests blocks via TRON's P2P protocol or RPC, decodes them into stable schemas, and emits structured block streams over gRPC — ready to feed into [Substreams](https://substreams.streamingfast.io/), Kafka, ClickHouse, `sigflow`, or your own indexer.

Where `java-tron` is built for full validation, `lightcycle` is built for getting clean, structured TRON data into downstream systems as fast and as cheaply as possible.

## Status

Pre-alpha. Active development. Schemas and APIs will change.

## Why

- **TRON tooling is stuck in 2019.** `java-tron` is heavy, TronGrid is rate-limited, and the public ecosystem has no Firehose-equivalent open-source streaming source.
- **Most TRON activity is USDT-TRC20.** Decoding it correctly should be the path of least resistance, not a research project.
- **Reorg-correct streams are table stakes** for indexers and bridges, and most TRON RPC tooling silently drops orphaned blocks.

## Design goals

1. **Firehose-protocol-compatible gRPC output** — Substreams works against `lightcycle` out of the box.
2. **Reorg-correct stream semantics** — every block carries `NEW`/`UNDO`/`IRREVERSIBLE`; cursors are fork-aware.
3. **Decoded data as a first-class output** — TRC-10/20/721, internal transactions, SR voting, freeze/unfreeze, USDT freeze/blacklist, all surfaced with stable schemas.
4. **No JVM dependency** — pure Rust, single binary; can talk to a remote `java-tron` over P2P or RPC.
5. **Fast backfill** — saturate NVMe on modern hardware, parallelize decode across cores.
6. **One-command quickstart** — `docker compose up` brings up the full local stack.

## Architecture (high-level)

```
TRON full node ──► source ──► codec ──► relayer ──► firehose gRPC ──► consumers
                  (P2P/RPC)  (decode,  (reorg,                      (Substreams,
                              verify)   finality)                     Kafka, sigflow…)
```

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) for component details and trust model.

## Quickstart

```bash
git clone https://github.com/<org>/lightcycle
cd lightcycle
docker compose -f docker/docker-compose.yml up
```

This brings up `java-tron` (chain peer), `lightcycle` (relayer), Prometheus + Grafana, and a sample Firehose consumer.

## CLI

```bash
# Live tail from the head of chain
lightcycle stream --source p2p://localhost:18888 --grpc-listen 0.0.0.0:13042

# Backfill from a height, then continue tailing
lightcycle stream --source p2p://localhost:18888 --start-block 60000000

# Inspect a single block (debugging)
lightcycle inspect --source rpc://localhost:8090 --block 60123456
```

## Benchmarking

See [`BENCHMARKS.md`](./BENCHMARKS.md) for the methodology, baselines, and harness. Headline expectations on modern hardware:

| Scenario              | `java-tron` event-sub | `lightcycle`    |
|-----------------------|-----------------------|-----------------|
| Idle RSS              | ~6–8 GB               | < 200 MB        |
| Live tail p50 latency | ~500 ms               | < 200 ms        |
| Cold backfill         | baseline              | ≥ 10× faster    |
| Reorg correctness     | partial               | first-class     |

## Layout

```
lightcycle/
├── crates/
│   ├── lightcycle-proto/      # generated protobuf types (TRON + Firehose)
│   ├── lightcycle-types/      # core domain types
│   ├── lightcycle-codec/      # block decode, sig verify, event decode
│   ├── lightcycle-source/     # P2P + RPC ingestion
│   ├── lightcycle-firehose/   # gRPC server speaking Firehose protocol
│   ├── lightcycle-store/      # block cache, cursor store
│   ├── lightcycle-relayer/    # orchestrator
│   └── lightcycle-cli/        # the binary
├── proto/                     # vendored .proto files
├── docker/                    # compose stack + Dockerfile
├── benches/                   # benchmark harness
├── ARCHITECTURE.md
├── BENCHMARKS.md
└── README.md
```

## Contributing

PRs welcome. Please read `ARCHITECTURE.md` first; the trust model and reorg semantics are load-bearing and changes there need design review.

## License

Dual-licensed under MIT or Apache 2.0, at your option.

## Acknowledgements

- TRON wire format from [`tronprotocol/java-tron`](https://github.com/tronprotocol/java-tron) (Apache 2.0)
- Firehose protocol from [`streamingfast/firehose-core`](https://github.com/streamingfast/firehose-core) (Apache 2.0)
- MPT and primitive types lifted from the [Alloy](https://github.com/alloy-rs/core) and [`reth`](https://github.com/paradigmxyz/reth) ecosystems
- Aesthetic, of course, from `TRON` (1982)
