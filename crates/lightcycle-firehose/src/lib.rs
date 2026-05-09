//! Firehose-protocol-compatible gRPC server.
//!
//! Speaks `sf.firehose.v2` so Substreams (and any other Firehose
//! consumer) can subscribe to live block streams with reorg-correct
//! step semantics. v0.1 ships **live mode only** — backfill via cursor
//! / start_block_num requires the [`lightcycle-store`] crate to be
//! wired in, which is a separate piece of work.
//!
//! Architecture in three pieces:
//!
//! - [`Hub`] multiplexes the relayer's single-consumer mpsc into a
//!   broadcast channel that fans out to all gRPC subscribers. Slow
//!   subscribers get `Lagged` rather than back-pressuring the engine.
//! - [`StreamService`] / [`EndpointInfoService`] — tonic impls of
//!   the Firehose v2 services.
//! - [`serve`] — wires everything into a tonic server bound to an
//!   address. The CLI's `relay --firehose-listen=...` flag is the
//!   typical caller.
//!
//! Out of scope for v0.1 (intentional, documented):
//! - `Fetch.Block` (point-in-time block lookup)
//! - `Stream.Blocks` backfill via `cursor` or `start_block_num`
//! - `final_blocks_only` and `transforms` request fields
//! - Chain-specific block payload (`Response.block` is currently a
//!   placeholder Any; consumers that need full block bytes should
//!   call the upstream node directly until the TRON-flavored
//!   `sf.tron.type.v1.Block` proto lands)

#![allow(dead_code)]

mod hub;
mod server;

pub use hub::Hub;
pub use server::{EndpointInfoService, StreamService};

use std::net::SocketAddr;

use anyhow::{Context, Result};
use lightcycle_proto::firehose::v2::{
    endpoint_info_server::EndpointInfoServer, stream_server::StreamServer,
};
use tonic::transport::Server;
use tracing::info;

/// Run the gRPC server until the shutdown future resolves.
///
/// `chain_name` is reflected in `EndpointInfo.Info` (e.g. "tron-mainnet");
/// `hub` provides the live broadcast for `Stream.Blocks`. The future
/// returned by `shutdown` (typically `tokio::signal::ctrl_c()`) drives
/// graceful shutdown.
pub async fn serve(
    listen: SocketAddr,
    hub: Hub,
    chain_name: impl Into<String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let stream_svc = StreamService::new(hub);
    let info_svc = EndpointInfoService::new(chain_name);

    info!(%listen, "firehose gRPC server starting");
    metrics::counter!("lightcycle_firehose_server_starts_total").increment(1);

    Server::builder()
        .add_service(StreamServer::new(stream_svc))
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
}
