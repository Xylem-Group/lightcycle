//! Firehose-protocol-compatible gRPC server.
//!
//! Speaks `bstream.v2` so Substreams (and any other Firehose consumer) can subscribe
//! to live + backfill block streams with reorg-correct undo semantics.

#![allow(dead_code)]

use std::net::SocketAddr;

/// Run the gRPC server, multiplexing one upstream block stream to many subscribers.
pub async fn serve(_listen: SocketAddr) -> anyhow::Result<()> {
    todo!("tonic gRPC server speaking sf.firehose.v2.Stream")
}
