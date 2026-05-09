//! Block oracle abstraction for `Fetch.Block`.
//!
//! `Stream.Blocks` reads from the live broadcast hub. `Fetch.Block` is a
//! point-in-time, request-driven RPC — the firehose has no live state for
//! a height the operator asks about. The oracle is the indirection that
//! bridges the gap.
//!
//! v0.1 contract:
//!
//! - The CLI builds a [`BlockOracle`] backed by a dedicated `GrpcSource`
//!   connection (separate from the relayer's source, which is owned by the
//!   live-tail pipeline). On each `Fetch.Block` call the oracle fetches
//!   the block from the upstream `Wallet.GetBlockByNum` RPC, decodes it,
//!   and returns the canonical envelope.
//! - There is **no caching** in v0.1. Each `Fetch.Block` is one upstream
//!   round-trip. When `lightcycle-store` grows a block cache, an in-memory
//!   `BlockOracle` impl will land there and the CLI will compose them
//!   (cache → upstream fallback) without changing the firehose surface.
//! - Finality is stamped from a snapshot of the chain's solidified head.
//!   For v0.1 the oracle takes one extra `WalletSolidity.GetNowBlock`
//!   round-trip per call; this lets `metadata.lib_num` be honest at the
//!   moment the response is built. When the cache lands, the cached
//!   solidified-head value will be reused.
//!
//! Why a trait and not a concrete struct: the firehose crate must stay
//! free of source-side dependencies (`tonic` channels, RPC error types).
//! Implementations live in the CLI (today) and `lightcycle-store` (later),
//! and tests substitute in-memory fakes.

use std::sync::Arc;

use async_trait::async_trait;
use lightcycle_relayer::BufferedBlock;
use lightcycle_types::BlockFinality;

/// What the `Fetch.Block` service needs from anything that can produce a
/// canonical block: the block in the same shape `encode_block` accepts,
/// plus the finality envelope to stamp on the response.
///
/// `Ok(None)` is the not-found state — the upstream chain doesn't have a
/// block at that height (likely too high). `Err(_)` is a server-side
/// failure (RPC, decode); the service surfaces it as `Status::internal`.
#[async_trait]
pub trait BlockOracle: Send + Sync + 'static {
    async fn fetch_block_by_number(
        &self,
        height: u64,
    ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>>;
}

/// Convenience alias — the `Arc<dyn BlockOracle>` shape the firehose
/// `serve` function takes. Keeps call sites readable.
pub type SharedBlockOracle = Arc<dyn BlockOracle>;
