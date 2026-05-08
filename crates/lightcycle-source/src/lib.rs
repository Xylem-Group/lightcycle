//! Block ingestion: P2P (preferred) and RPC (fallback) modes.

#![allow(dead_code)]

use async_trait::async_trait;
use futures::Stream;
use lightcycle_types::BlockHeight;
use std::pin::Pin;

/// A raw block as received from the upstream — protobuf bytes plus minimal metadata.
#[derive(Debug, Clone)]
pub struct RawBlock {
    pub height: BlockHeight,
    pub bytes: Vec<u8>,
}

/// A source produces an ordered stream of raw blocks.
#[async_trait]
pub trait Source: Send + Sync {
    /// Stream blocks starting from `from`, or from current head if `from` is `None`.
    async fn stream_from(
        &self,
        from: Option<BlockHeight>,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<RawBlock>> + Send>>>;
}

pub mod p2p {
    //! TRON P2P client. Speaks the protobuf-over-TCP protocol used by `java-tron`.
}

pub mod rpc {
    //! RPC fallback. Polls `java-tron` HTTP/gRPC. Higher latency, simpler ops.
}
