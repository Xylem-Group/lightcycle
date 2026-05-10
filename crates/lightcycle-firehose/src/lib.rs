//! Firehose-protocol-compatible gRPC server.
//!
//! Speaks `sf.firehose.v2` so Substreams (and any other Firehose
//! consumer) can subscribe to live block streams with reorg-correct
//! step semantics. Ships `Stream.Blocks` (live tail + in-cache
//! backfill via `start_block_num` or `cursor`), `Fetch.Block`
//! (point-in-time by number), and `EndpointInfo.Info`. Backfill
//! beyond the in-memory cache window requires a persistent block
//! archive, which is a separate piece of work.
//!
//! Architecture in four pieces:
//!
//! - [`Hub`] multiplexes the relayer's single-consumer mpsc into a
//!   broadcast channel that fans out to all gRPC subscribers. Slow
//!   subscribers get `Lagged` rather than back-pressuring the engine.
//! - [`StreamService`] / [`FetchService`] / [`EndpointInfoService`] —
//!   tonic impls of the Firehose v2 services.
//! - [`BlockOracle`] — abstraction Fetch.Block consumes. The CLI builds
//!   a concrete impl over a dedicated `GrpcSource`; future
//!   `lightcycle-store` cache impls compose without changing the
//!   firehose surface.
//! - [`serve`] — wires everything into a tonic server bound to an
//!   address. The CLI's `relay --firehose-listen=...` flag is the
//!   typical caller.
//!
//! Out of scope for v0.1 (intentional, documented):
//! - Backfill beyond the in-memory cache window — gated on a
//!   persistent block archive (redb spill from `lightcycle-store`).
//! - `Fetch.Block` references other than `BlockNumber` (no
//!   `BlockHashAndNumber`, no `Cursor`); no `transforms`.
//! - `final_blocks_only` and `transforms` on Stream.

#![allow(dead_code)]

mod encode;
mod hub;
mod oracle;
mod server;

pub use encode::{encode_block, BLOCK_TYPE_URL};
pub use hub::Hub;
pub use oracle::{describe_oracle_metrics, BlockOracle, CachingBlockOracle, SharedBlockOracle};
pub use server::{EndpointInfoService, FetchService, StreamService};

// Backfill metrics — described here so they show up before the
// first request.
pub fn describe_backfill_metrics() {
    metrics::describe_counter!(
        "lightcycle_firehose_backfill_total",
        "Stream.Blocks backfill walks, labelled by result (ok / out_of_window / cache_miss)"
    );
    metrics::describe_histogram!(
        "lightcycle_firehose_backfill_blocks",
        "Number of blocks emitted from the in-memory cache during a Stream.Blocks backfill."
    );
}

use std::net::SocketAddr;

use anyhow::{Context, Result};
use lightcycle_proto::firehose::v2::{
    endpoint_info_server::EndpointInfoServer, fetch_server::FetchServer,
    stream_server::StreamServer,
};
use lightcycle_relayer::BufferedBlock;
use lightcycle_store::SharedBlockCache;
use lightcycle_types::BlockHeight;
use tokio::sync::watch;
use tonic::transport::Server;
use tracing::info;

/// Optional backfill wiring for [`serve`]. When both fields are
/// `Some`, `Stream.Blocks` honors `start_block_num` and `cursor`
/// requests by walking the cache before joining the live broadcast.
/// When either is `None`, requests with start hints get rejected
/// with `FailedPrecondition`.
#[derive(Clone)]
pub struct StreamBackfill {
    pub cache: SharedBlockCache<BufferedBlock>,
    pub solidified_head: watch::Receiver<Option<BlockHeight>>,
}

impl std::fmt::Debug for StreamBackfill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamBackfill").finish_non_exhaustive()
    }
}

/// Run the gRPC server until the shutdown future resolves.
///
/// `chain_name` is reflected in `EndpointInfo.Info` (e.g. "tron-mainnet");
/// `hub` provides the live broadcast for `Stream.Blocks`; `oracle` backs
/// `Fetch.Block`. `backfill` is the optional cache + head watch that
/// powers `Stream.Blocks` resume — pass `None` to run live-tail-only.
/// The future returned by `shutdown` (typically `tokio::signal::ctrl_c()`)
/// drives graceful shutdown.
pub async fn serve(
    listen: SocketAddr,
    hub: Hub,
    oracle: SharedBlockOracle,
    chain_name: impl Into<String>,
    backfill: Option<StreamBackfill>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let stream_svc = match backfill {
        Some(b) => StreamService::new(hub).with_block_cache(b.cache, b.solidified_head),
        None => StreamService::new(hub),
    };
    let fetch_svc = FetchService::new(oracle);
    let info_svc = EndpointInfoService::new(chain_name);

    info!(%listen, "firehose gRPC server starting");
    metrics::counter!("lightcycle_firehose_server_starts_total").increment(1);

    Server::builder()
        .add_service(StreamServer::new(stream_svc))
        .add_service(FetchServer::new(fetch_svc))
        .add_service(EndpointInfoServer::new(info_svc))
        .serve_with_shutdown(listen, shutdown)
        .await
        .with_context(|| format!("firehose gRPC server on {listen} exited with error"))?;

    info!("firehose gRPC server stopped");
    Ok(())
}

/// Describe firehose-side metrics so they appear in Prometheus output
/// from process startup.
pub fn describe_metrics() {
    metrics::describe_counter!(
        "lightcycle_firehose_subscriptions_total",
        "total firehose Stream.Blocks subscriptions opened (lifetime)"
    );
    metrics::describe_gauge!(
        "lightcycle_firehose_subscribers",
        "current count of attached Stream.Blocks subscribers"
    );
    metrics::describe_counter!(
        "lightcycle_firehose_outputs_total",
        "outputs propagated through the hub, labelled by result"
    );
    metrics::describe_counter!(
        "lightcycle_firehose_lagged_total",
        "subscriber-lag events; means a consumer fell behind the broadcast buffer"
    );
    metrics::describe_counter!(
        "lightcycle_firehose_server_starts_total",
        "firehose gRPC server start events (process restarts)"
    );
    metrics::describe_counter!(
        "lightcycle_firehose_fetch_total",
        "Fetch.Block requests, labelled by result (in_flight / ok / not_found / error)"
    );
    metrics::describe_histogram!(
        "lightcycle_firehose_fetch_duration_seconds",
        metrics::Unit::Seconds,
        "Fetch.Block request duration. Includes upstream RPC + decode time."
    );
}
