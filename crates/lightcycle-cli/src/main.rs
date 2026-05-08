//! lightcycle — a streaming relayer for the TRON grid.

use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "lightcycle", version, about = "TRON streaming relayer")]
struct Cli {
    /// Log filter (e.g. "lightcycle=info,warn").
    #[arg(long, env = "RUST_LOG", default_value = "lightcycle=info")]
    log: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the relayer (live tail, optional backfill).
    Stream(StreamArgs),
    /// Inspect a single block (debugging).
    Inspect(InspectArgs),
    /// Print version and feature flags.
    Version,
}

#[derive(Parser, Debug)]
struct StreamArgs {
    /// Source URI (e.g. "p2p://host:port" or "rpc://host:port").
    #[arg(long, env = "LIGHTCYCLE_SOURCE")]
    source: String,

    /// Address to bind the Firehose gRPC server.
    #[arg(long, env = "LIGHTCYCLE_GRPC_LISTEN", default_value = "0.0.0.0:13042")]
    grpc_listen: SocketAddr,

    /// Address to bind the Prometheus metrics endpoint.
    #[arg(long, env = "LIGHTCYCLE_METRICS_LISTEN", default_value = "0.0.0.0:9100")]
    metrics_listen: SocketAddr,

    /// Optional backfill start height. If unset, tail from current head.
    #[arg(long)]
    start_block: Option<u64>,
}

#[derive(Parser, Debug)]
struct InspectArgs {
    /// Block height to fetch and decode.
    #[arg(long)]
    block: u64,

    /// Source URI.
    #[arg(long, env = "LIGHTCYCLE_SOURCE")]
    source: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_target(true)
        .json()
        .init();

    match cli.cmd {
        Cmd::Stream(args) => run_stream(args).await,
        Cmd::Inspect(args) => run_inspect(args).await,
        Cmd::Version => {
            println!("lightcycle {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

async fn run_stream(args: StreamArgs) -> Result<()> {
    tracing::info!(?args, "starting relayer");
    // TODO: wire lightcycle-source -> codec -> relayer -> firehose-server.
    anyhow::bail!("not yet implemented")
}

async fn run_inspect(args: InspectArgs) -> Result<()> {
    tracing::info!(?args, "inspecting block");
    // TODO: open source, fetch block by height, decode, pretty-print.
    anyhow::bail!("not yet implemented")
}
