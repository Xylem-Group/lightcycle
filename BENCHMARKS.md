# Benchmarks

How we measure `lightcycle`, what we compare against, and how to reproduce results.

## Reference hardware

The numbers committed to this repo are gathered on:

- **CPU**: AMD EPYC 4564P, 16C/32T, 5.88 GHz boost (Zen 4)
- **RAM**: 128 GB DDR5
- **Storage**: 4× NVMe — 2× Micron 7450 480 GB (boot + logs), 2× Micron 7450 PRO 1.92 TB (chain data + relayer)
- **Network**: 2× 10 Gbps
- **OS**: Ubuntu 24.04.4 LTS, kernel 6.8.0-111

Single-thread frequency on this CPU is excellent for our hot paths (protobuf decode + secp256k1 signature recovery), and 10 Gbps networking means we will not be network-bound for any realistic test. The expected bottleneck profile:

- **Backfill**: CPU-bound on decode + sigverify
- **Live tail**: latency-bound on the source's emission cadence
- **Steady state at low fanout**: essentially free

## Isolation and methodology

Before each run:

```bash
# Pin CPU governor to performance
sudo cpupower frequency-set -g performance

# Drop OS page cache (for cold benchmarks)
sudo sync && echo 3 | sudo tee /proc/sys/vm/drop_caches

# Disable transparent hugepages (avoids latency spikes)
echo never | sudo tee /sys/kernel/mm/transparent_hugepage/enabled

# Pin processes to disjoint CPU sets
taskset -c 0-15  lightcycle ...
taskset -c 16-31 java-tron  ...
```

Storage layout for head-to-head runs:

- `java-tron` chain data → `/mnt/nvme1` (Micron 7450 PRO #1)
- `lightcycle` cache + cursor store → `/mnt/nvme2` (Micron 7450 PRO #2)
- Logs and Prometheus → `/mnt/nvme0` (one of the 480GB drives)

This keeps I/O contention from skewing comparisons.

Each benchmark is run 3× and we report the median. Cold/warm distinction is called out per scenario.

## Baselines

We compare against two reference setups:

1. **`java-tron` + event-subscribe → Kafka.** The "official" path for streaming TRON data. `java-tron` runs with `event.subscribe.enable=true`, writing to a local single-broker Kafka. Consumers read from Kafka.
2. **TronGrid HTTP polling.** The lazy path: poll TronGrid's `/wallet/getnowblock` and walk back, diffing against the previous high-water mark. What most projects actually do today.

Both baselines have known weaknesses (`java-tron` is heavyweight; TronGrid is rate-limited and not reorg-correct). We measure them anyway because they're what people compare against.

## Scenarios

### B1 — Block-to-Kafka latency (live tail)

**Measures**: end-to-end latency from new block production on TRON mainnet to durable commit in a downstream Kafka topic.

**Setup**: `lightcycle` and `java-tron`-event-subscribe both running, both writing to separate Kafka topics. Each block carries TRON's own block timestamp; we measure `kafka_commit_time - block.timestamp`.

**Metric**: p50, p95, p99 latency over a 24h window.

**Hypothesis**: `lightcycle` p50 < 200 ms (one network hop + decode + gRPC + Kafka commit). `java-tron` event-subscribe p50 ~500 ms (heavier internal pipeline).

### B2 — Cold backfill throughput

**Measures**: how fast we sync historical data from a clean disk.

**Setup**: starting from a clean disk and cold OS cache, sync from height H₀ to H₀ + 1,000,000 blocks (~35 days of mainnet, ~290M transactions, ~120M USDT-TRC20 transfers in a busy window).

**Metric**: wall time, blocks/sec, decoded events/sec, peak RSS, peak CPU%.

**Hypothesis**: `lightcycle` ≥ 5,000 blocks/sec sustained on this hardware (CPU-bound on decode + sigverify). `java-tron` event-subscribe is far slower because it does full state execution; expect 10–50× slower.

### B3 — Decode hot-path microbench

**Measures**: pure CPU throughput of the protobuf-decode + signature-verify + event-decode pipeline, isolated from I/O and network.

**Setup**: 10,000 known blocks pre-loaded from disk, ranging across busy and quiet windows. Run with `criterion`.

**Metric**: ns/block decoded, ns/transaction decoded, ns/log decoded. Reported per-core.

**Why**: tells us our headroom on a single core, which is the real constraint at sustained backfill.

### B4 — Reorg correctness

**Measures**: does the relayer emit correct undo events when the canonical chain switches?

**Setup**: replay a recorded mainnet fork (TRON sees small reorgs regularly; we capture one and replay it through the source layer). Verify the consumer sees `UNDO` for each orphaned block before `NEW` for the new branch, and that all data — addresses, amounts, log topics — is consistent end to end.

**Metric**: pass/fail. Reorg detection latency.

### B5 — Idle resource baseline

**Measures**: footprint when running but with no consumers, just keeping head fresh.

**Setup**: run for 1 hour with no gRPC clients connected. Sample RSS and CPU% every 10 s.

**Metric**: median RSS, median CPU%.

**Hypothesis**: `lightcycle` < 200 MB RSS, < 2% of one core. `java-tron` event-subscribe ~6–8 GB RSS, ~10% CPU.

### B6 — Saturation point (synthetic load)

**Measures**: at what input block rate does latency depart from baseline?

**Setup**: against a private TRON network, produce blocks at 1×, 2×, 5×, 10×, 20× mainnet rate. Measure decode-pipeline lag.

**Metric**: input rate at which p99 latency exceeds 1 s.

**Why it matters**: tells us our headroom for future chain growth or burst conditions.

### B7 — Consumer fanout

**Measures**: does adding consumers degrade per-consumer latency?

**Setup**: 1, 10, 100, 1000 gRPC consumers all subscribed to live tail. Measure per-consumer p99 latency.

**Metric**: latency vs. consumer count curve.

**Hypothesis**: flat to 100 consumers, gentle degradation to 1000 (we become CPU-bound on serialization at high fanout).

### B8 — Resumption correctness

**Measures**: cursor-based resumption after process kill.

**Setup**: stream live, `kill -9` mid-block, restart, verify the consumer receives no gaps and no duplicates.

**Metric**: pass/fail, plus measured restart-to-resume latency.

### B9 — Decoder accuracy

**Measures**: does our decoded output match a trusted reference?

**Setup**: take 1M random recent transactions. Pull our decoded output and TronGrid's `getTransactionInfoById` output for the same txs. Diff structurally.

**Metric**: number of mismatches, root-caused individually.

**Why**: silent decode bugs are the worst possible failure mode for an indexer source. We need ≈ 0 mismatches as a release gate.

### B10 — 7-day soak

**Measures**: memory leaks, file descriptor leaks, slow degradations.

**Setup**: run live tail + sample backfill + 50 connected consumers for 7 days continuous. Plot RSS, FD count, CPU% over time.

**Metric**: any unbounded growth.

## Tooling

- **Microbenchmarks**: `criterion` (`cargo bench`).
- **End-to-end harness**: custom Rust binary in `benches/harness/`, scenario-driven via TOML configs.
- **Metrics collection**: `metrics-exporter-prometheus` baked into both `lightcycle` and the harness; Prometheus + Grafana for time-series capture.
- **Pre-built dashboard**: `docker/grafana/dashboards/lightcycle.json` provides the standard view (latency, throughput, RSS, FD count, reorg events).

## Reading results

Don't fixate on absolute numbers — they vary with mainnet load. Focus on:

- The **delta vs. baseline** under matched conditions (B1, B2, B5).
- The **shape of degradation** under stress (B6, B7, B10).
- **Correctness pass rates** — a fast incorrect relayer is worse than a slow correct one (B4, B8, B9).

## Reproducibility

All benchmarks land in `benches/` with the harness and scenario configs. Results are committed under `benches/results/<date>/<hardware-tag>/` along with the kernel version, CPU governor settings, and the `java-tron` build hash used as baseline. PRs adding results from new hardware are welcome — please include the same metadata.
