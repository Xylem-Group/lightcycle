//! lightcycle — a streaming relayer for the TRON grid.
//!
//! v0.1 surface: `stream` runs an RPC head-poller that pulls
//! `/wallet/getnowblock` from a configured java-tron HTTP endpoint and
//! exposes Prometheus metrics. The full relayer pipeline (P2P source,
//! reorg engine, Firehose gRPC server) lands incrementally; this CLI
//! binary grows with it.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lightcycle_source::HeadPoller;
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "lightcycle", version, about = "TRON streaming relayer")]
struct Cli {
    /// Log filter (e.g. "lightcycle=info,warn").
    #[arg(long, env = "RUST_LOG", default_value = "lightcycle=info,warn")]
    log: String,

    /// Emit logs as plain text (default) or JSON.
    #[arg(long, default_value = "text")]
    log_format: LogFormat,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the relayer (head-tracking only in v0.1; full pipeline is incremental).
    Stream(StreamArgs),
    /// Inspect a single block over RPC.
    Inspect(InspectArgs),
    /// Print version and feature flags.
    Version,
}

#[derive(Parser, Debug)]
struct StreamArgs {
    /// java-tron HTTP RPC base URL. Default targets a local node.
    /// Use `http://127.0.0.1:8090` after `ssh -L 8090:127.0.0.1:8090 xylem-tokyo`.
    #[arg(
        long,
        env = "LIGHTCYCLE_RPC_URL",
        default_value = "http://127.0.0.1:8090"
    )]
    rpc_url: String,

    /// Address to bind the Prometheus `/metrics` endpoint.
    /// Pick a port distinct from java-tron's exporter (9527) so both
    /// can be scraped concurrently — 9528 is the kulen convention.
    #[arg(
        long,
        env = "LIGHTCYCLE_METRICS_LISTEN",
        default_value = "127.0.0.1:9528"
    )]
    metrics_listen: SocketAddr,

    /// How often to poll the upstream RPC for the current head.
    /// Mainnet block production is ~3 s; polling faster than that
    /// just adds work without resolution. Seconds, integer.
    #[arg(long, env = "LIGHTCYCLE_POLL_INTERVAL_SECS", default_value_t = 3)]
    poll_interval_secs: u64,
}

#[derive(Parser, Debug)]
struct InspectArgs {
    /// Block height to fetch and decode.
    #[arg(long)]
    block: u64,

    /// java-tron HTTP RPC base URL.
    #[arg(long, env = "LIGHTCYCLE_RPC_URL")]
    rpc_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log, cli.log_format);

    match cli.cmd {
        Cmd::Stream(args) => run_stream(args).await,
        Cmd::Inspect(args) => run_inspect(args).await,
        Cmd::Version => {
            println!("lightcycle {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

fn init_tracing(filter: &str, format: LogFormat) {
    let env = EnvFilter::new(filter);
    match format {
        LogFormat::Json => fmt().with_env_filter(env).with_target(true).json().init(),
        LogFormat::Text => fmt().with_env_filter(env).with_target(true).init(),
    }
}

async fn run_stream(args: StreamArgs) -> Result<()> {
    // Install the Prometheus exporter FIRST so the gauges/counters
    // describe-statements below land in the same handle and the very
    // first scrape (which can race the first poll) shows zero values
    // instead of empty.
    PrometheusBuilder::new()
        .with_http_listener(args.metrics_listen)
        .install()
        .context("install prometheus exporter")?;
    lightcycle_source::rpc::describe_metrics();
    metrics::describe_gauge!(
        "lightcycle_info",
        "build info: always 1, with version + feature labels"
    );
    metrics::gauge!("lightcycle_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    let interval = Duration::from_secs(args.poll_interval_secs.max(1));
    tracing::info!(
        rpc_url = %args.rpc_url,
        metrics_listen = %args.metrics_listen,
        poll_interval_secs = args.poll_interval_secs,
        "starting RPC head poller"
    );

    let (poller, mut rx) = HeadPoller::new(args.rpc_url, interval);

    // Spawn the poller; drop the JoinHandle — Tokio cancels on drop is
    // fine here because we abort on Ctrl-C below anyway.
    let _poller_handle = tokio::spawn(poller.run());

    // Light-touch heartbeat task: log the latest head every 30s so a
    // tail of `journalctl -u lightcycle` shows liveness without
    // requiring metrics access. Runs concurrently with the poller.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await; // arm the ticker (first tick is immediate).

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if let Some(info) = rx.borrow().clone() {
                    tracing::info!(
                        height = info.height,
                        timestamp_ms = info.timestamp_ms,
                        block_id = %&info.block_id_hex[..16.min(info.block_id_hex.len())],
                        "heartbeat: current head"
                    );
                } else {
                    tracing::warn!("heartbeat: no head observed yet");
                }
            }
            // Wait for any update on the watch channel — useful for ops
            // who want a structured-log entry per-block in the future.
            // For now we just consume the notification to keep the
            // channel from growing stale.
            _ = rx.changed() => {}
            _ = &mut shutdown => {
                tracing::info!("ctrl-c received, shutting down");
                break;
            }
        }
    }
    Ok(())
}

async fn run_inspect(args: InspectArgs) -> Result<()> {
    tracing::info!(?args, "inspect not yet wired up");
    anyhow::bail!(
        "inspect requires the full block-fetch RPC path (gRPC GetBlockByNum). \
         Vendor google/api/annotations.proto and re-enable tron/api/ codegen \
         to land this; tracking issue: SCAFFOLD.md priority 4."
    )
}
