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
use lightcycle_relayer::{
    GrpcBlockFetcher, RelayerService, ReorgConfig, ReorgEngine, VerifyPolicy,
};
use lightcycle_source::{GrpcSource, HeadPoller};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "lightcycle", version, about = "TRON streaming relayer")]
struct Cli {
    /// Log filter (e.g. "lightcycle=info,warn"). `global` so it's accepted
    /// at any position; the kulen oci-container module passes args after
    /// the subcommand because that's the natural shape for an
    /// argv-shaped `cmd = [...]` list.
    #[arg(long, env = "RUST_LOG", default_value = "lightcycle=info,warn", global = true)]
    log: String,

    /// Emit logs as plain text (default) or JSON. Same `global` rationale
    /// as `log` above.
    #[arg(long, default_value = "text", global = true)]
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
    /// Run the lightweight head poller (HTTP RPC, head metadata only).
    /// Cheap and works against any node including lite-fullnode; useful
    /// for dashboards and the kulen comparison panel. For the full
    /// decode + verify + reorg pipeline use `relay`.
    Stream(StreamArgs),
    /// Run the live ingest pipeline: fetch blocks via gRPC, decode,
    /// verify witness signatures, drive the reorg engine, log emitted
    /// step events. The flagship subcommand.
    Relay(RelayArgs),
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
struct RelayArgs {
    /// java-tron gRPC endpoint. Default targets the local node via
    /// loopback. Same shape as `inspect` — the relay path uses the
    /// same `Wallet.GetNowBlock` RPC.
    #[arg(
        long,
        env = "LIGHTCYCLE_GRPC_URL",
        default_value = "http://127.0.0.1:50051"
    )]
    grpc_url: String,

    /// Address to bind the Prometheus `/metrics` endpoint. Pick a port
    /// distinct from `stream`'s exporter (also 9528 by default) so both
    /// can coexist if you want to compare paths.
    #[arg(
        long,
        env = "LIGHTCYCLE_RELAY_METRICS_LISTEN",
        default_value = "127.0.0.1:9529"
    )]
    metrics_listen: SocketAddr,

    /// Poll interval, milliseconds. TRON blocks are ~3s; 1000ms gives
    /// us 3 chances to catch each. Faster polling does not improve
    /// latency below the upstream's block production rate but does
    /// add load.
    #[arg(long, env = "LIGHTCYCLE_RELAY_POLL_INTERVAL_MS", default_value_t = 1000)]
    poll_interval_ms: u64,

    /// Verify policy: `disabled` skips sigverify entirely; `lenient`
    /// (default) accepts SM2-class blocks (~25% of mainnet) under the
    /// documented pass-through caveat; `strict` rejects them.
    #[arg(long, env = "LIGHTCYCLE_RELAY_VERIFY", default_value = "lenient")]
    verify: VerifyPolicyArg,

    /// Reorg engine: how many canonical blocks to keep in the live
    /// buffer. Must exceed `--finality-depth`. 30 covers any realistic
    /// TRON reorg.
    #[arg(long, env = "LIGHTCYCLE_RELAY_BUFFER_WINDOW", default_value_t = 30)]
    buffer_window: usize,

    /// Reorg engine: confirmations until a block is emitted as
    /// `Irreversible`. TRON's solidified rule is 19/27 SR confirmations.
    #[arg(long, env = "LIGHTCYCLE_RELAY_FINALITY_DEPTH", default_value_t = 19)]
    finality_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum VerifyPolicyArg {
    Disabled,
    Lenient,
    Strict,
}

impl From<VerifyPolicyArg> for VerifyPolicy {
    fn from(v: VerifyPolicyArg) -> Self {
        match v {
            VerifyPolicyArg::Disabled => VerifyPolicy::Disabled,
            VerifyPolicyArg::Lenient => VerifyPolicy::Lenient,
            VerifyPolicyArg::Strict => VerifyPolicy::Strict,
        }
    }
}

#[derive(Parser, Debug)]
struct InspectArgs {
    /// Block height to fetch and decode. Omit to fetch the current
    /// head — which is the only option for nodes running LiteFullNode
    /// mode (the default per ADR 0012); historical `getblockbynum` is
    /// closed in that mode.
    #[arg(long)]
    block: Option<u64>,

    /// java-tron gRPC endpoint. Default targets the local node via
    /// loopback. Different from `stream`'s --rpc-url (which is HTTP);
    /// gRPC is on a separate port (50051 default).
    #[arg(
        long,
        env = "LIGHTCYCLE_GRPC_URL",
        default_value = "http://127.0.0.1:50051"
    )]
    grpc_url: String,

    /// Optional path to dump the encoded `Block` proto bytes (for
    /// capturing real-mainnet test fixtures). The encoded bytes are
    /// what `lightcycle_codec::decode_block(&[u8])` consumes.
    #[arg(long)]
    dump_to: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log, cli.log_format);

    match cli.cmd {
        Cmd::Stream(args) => run_stream(args).await,
        Cmd::Relay(args) => run_relay(args).await,
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

async fn run_relay(args: RelayArgs) -> Result<()> {
    // Stand the metrics exporter up first so the very first scrape
    // doesn't race the first poll.
    PrometheusBuilder::new()
        .with_http_listener(args.metrics_listen)
        .install()
        .context("install prometheus exporter")?;
    lightcycle_relayer::describe_metrics();
    metrics::describe_gauge!(
        "lightcycle_info",
        "build info: always 1, with version + feature labels"
    );
    metrics::gauge!(
        "lightcycle_info",
        "version" => env!("CARGO_PKG_VERSION"),
        "subcommand" => "relay",
    )
    .set(1.0);

    let verify_policy: VerifyPolicy = args.verify.into();

    tracing::info!(
        grpc_url = %args.grpc_url,
        metrics_listen = %args.metrics_listen,
        poll_interval_ms = args.poll_interval_ms,
        verify_policy = ?verify_policy,
        buffer_window = args.buffer_window,
        finality_depth = args.finality_depth,
        "starting relay pipeline"
    );

    // Connect, fetch the initial SR set if we'll be verifying. SR-set
    // refresh-on-epoch-rotation is a follow-up; for now the set is
    // pinned at startup, which works for runs shorter than a TRON
    // maintenance period (~6h).
    let mut source = GrpcSource::connect(args.grpc_url.clone())
        .await
        .with_context(|| format!("gRPC connect: {}", args.grpc_url))?;

    let sr_set = if verify_policy != VerifyPolicy::Disabled {
        let set = source
            .fetch_active_sr_set()
            .await
            .context("fetch initial SR set")?;
        tracing::info!(size = set.len(), "fetched active SR set");
        Some(set)
    } else {
        tracing::info!("verify disabled; skipping SR set fetch");
        None
    };

    let engine = ReorgEngine::new(ReorgConfig {
        buffer_window: args.buffer_window,
        finality_depth: args.finality_depth,
    });
    let fetcher = GrpcBlockFetcher::new(source);
    let service = RelayerService::new(
        fetcher,
        engine,
        sr_set,
        verify_policy,
        std::time::Duration::from_millis(args.poll_interval_ms.max(50)),
    );

    // The service is the producer; we drain the receiver and currently
    // just log. Once the firehose gRPC server lands, this is where its
    // multiplex hub will subscribe.
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(256);
    let service_handle = tokio::spawn(service.run(output_tx));

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    // Drain emitted Outputs alongside the shutdown signal. The service
    // already logs each block; we don't double-log here. The drain
    // exists to (a) keep the channel non-full so the service doesn't
    // back-pressure on send, and (b) act as the obvious wiring point
    // for the firehose subscription manager.
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("ctrl-c received, shutting down relay");
                break;
            }
            recv = output_rx.recv() => {
                match recv {
                    Some(_output) => {} // logged inside the service
                    None => {
                        tracing::warn!("relay service exited");
                        break;
                    }
                }
            }
        }
    }
    drop(output_rx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), service_handle).await;
    Ok(())
}

async fn run_inspect(args: InspectArgs) -> Result<()> {
    use lightcycle_codec::{decode_block_message, ContractKind};
    use lightcycle_source::GrpcSource;
    use prost::Message;
    use std::collections::BTreeMap;

    tracing::debug!(?args, "inspect");

    let mut source = GrpcSource::connect(args.grpc_url.clone())
        .await
        .with_context(|| format!("connecting to {}", args.grpc_url))?;

    let block = match args.block {
        Some(h) => source
            .fetch_block(h)
            .await
            .with_context(|| format!("fetching block {h}"))?,
        None => source
            .fetch_now_block()
            .await
            .context("fetching current head")?,
    };

    if let Some(path) = args.dump_to.as_ref() {
        // Round-trip through encode_to_vec to get the canonical wire
        // representation — what unit tests + fixture-replay code use.
        let bytes = block.encode_to_vec();
        std::fs::write(path, &bytes).with_context(|| format!("write {}", path.display()))?;
        tracing::info!(
            path = %path.display(),
            bytes = bytes.len(),
            "dumped block proto"
        );
    }

    let decoded = decode_block_message(&block).context("codec decode failed")?;

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for tx in &decoded.transactions {
        for c in &tx.contracts {
            let key = match c.kind() {
                ContractKind::Other(tag) => format!("Other({tag})"),
                k => format!("{k:?}"),
            };
            *by_kind.entry(key).or_default() += 1;
        }
    }

    println!("height           : {}", decoded.header.height);
    println!("block_id         : 0x{}", hex::encode(decoded.header.block_id.0));
    println!("parent_id        : 0x{}", hex::encode(decoded.header.parent_id.0));
    println!(
        "tx_trie_root     : 0x{}",
        hex::encode(decoded.header.tx_trie_root)
    );
    println!("timestamp_ms     : {}", decoded.header.timestamp_ms);
    println!("witness          : 0x{}", hex::encode(decoded.header.witness.0));
    println!("witness_sig_len  : {} bytes", decoded.header.witness_signature.len());
    println!("version          : {}", decoded.header.version);
    println!("transactions     : {}", decoded.transactions.len());
    if !by_kind.is_empty() {
        println!("contract breakdown:");
        for (kind, n) in &by_kind {
            println!("  {n:>5}  {kind}");
        }
    }
    Ok(())
}
