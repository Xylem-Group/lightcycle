# Benchmark harness

See [`../BENCHMARKS.md`](../BENCHMARKS.md) for methodology and scenario list.

## Layout

```
benches/
├── harness/             # end-to-end scenario runner (B1, B2, B6, B7, B10)
├── microbench/          # criterion benches for codec hot paths (B3)
├── correctness/         # reorg + resumption + decoder accuracy tests (B4, B8, B9)
├── results/             # committed results, organized by date + hardware tag
│   └── README.md
└── scenarios/           # TOML configs consumed by the harness
    ├── b1-live-tail.toml
    ├── b2-cold-backfill.toml
    └── …
```

## Running

```bash
# microbenches
cargo bench -p lightcycle-codec

# end-to-end harness
cargo run --release -p lightcycle-harness -- --scenario benches/scenarios/b1-live-tail.toml
```

Results land in `benches/results/<YYYY-MM-DD>/<hardware-tag>/` along with the
hardware fingerprint, kernel version, and build hashes used.
