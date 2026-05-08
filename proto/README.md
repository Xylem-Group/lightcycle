# Vendored protobuf definitions

This directory holds protobuf schemas vendored from upstream projects. We vendor (rather than git-submodule) to keep builds reproducible without network access.

## tron/

Sourced from [`tronprotocol/java-tron`](https://github.com/tronprotocol/java-tron) at commit `87baadabca951981c1188abcf548de9dfafb36a3`.
Defines the wire format for blocks, transactions, contract types, and the P2P protocol.

License: Apache 2.0.

## firehose/

Sourced from [`streamingfast/proto`](https://github.com/streamingfast/proto) at commit `<TBD>`.
Defines the `sf.firehose.v2` streaming protocol that downstream consumers (Substreams, etc.) speak.

The schema lives in `streamingfast/proto`; the Go runtime (`streamingfast/firehose-core`) and the streaming engine (`streamingfast/bstream`) consume it. We vendor the schema only — neither the Go runtime nor the firehose-core CLI are dependencies.

License: Apache 2.0.

## Updating

```bash
./scripts/sync-protos.sh
```

This re-fetches both upstreams at pinned commits, copies the relevant `.proto` files, and writes the new commit SHAs back into this README.
