//! Block ingestion: P2P (preferred, future) and RPC (fallback, current).
//!
//! v0.1 ships a head-polling RPC source. The full `Source` trait — which
//! streams a continuous tail of `RawBlock`s — is intentionally not yet
//! implemented for the RPC path; the HTTP API returns JSON, not raw
//! protobuf bytes, and round-tripping JSON→Block proto→`DecodedBlock`
//! adds lossy hops we'd rather avoid. P2P (which delivers raw protobuf
//! naturally) is the right home for the streaming surface; RPC stays
//! head-tracking-only until that lands.
//!
//! What the RPC head poller IS good for today:
//!   - Comparing lightcycle's view of the chain head against what
//!     java-tron is reporting on the same box (the comparison
//!     dashboard, kulen-side).
//!   - Driving the `Source` trait's `current_head()` method for any
//!     consumer that just wants "where is mainnet right now."
//!   - Exercising the metrics + ops surface end-to-end before the
//!     heavier P2P + codec wiring lands.

#![allow(dead_code)]

use async_trait::async_trait;
use futures::Stream;
use lightcycle_types::BlockHeight;
use std::pin::Pin;

pub mod p2p;
pub mod rpc;

pub use rpc::{HeadInfo, HeadPoller};

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
