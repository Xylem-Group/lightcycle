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

    /// SR-set refresh interval, seconds. TRON's maintenance period is
    /// 7,200 blocks ≈ 6h; refreshing every 30 minutes catches an SR
    /// rotation without stale-set risk. Set to 0 to disable refresh
    /// (one-shot fetch at startup, suitable for runs <6h).
    #[arg(
        long,
        env = "LIGHTCYCLE_RELAY_SR_REFRESH_SECS",
        default_value_t = 1800
    )]
    sr_refresh_secs: u64,

    /// Bind the Firehose v2 gRPC server here. When set, every
    /// emitted Output is broadcast to attached `Stream.Blocks`
    /// subscribers. When unset, the relay runs in log-only mode
    /// (engine still drives, just no consumers).
    #[arg(long, env = "LIGHTCYCLE_RELAY_FIREHOSE_LISTEN")]
    firehose_listen: Option<SocketAddr>,

    /// Chain identity reported by `EndpointInfo.Info`. Defaults to
    /// "tron-mainnet" since that's what the LIGHTCYCLE_GRPC_URL
    /// default targets; override for testnets.
    #[arg(
        long,
        env = "LIGHTCYCLE_RELAY_CHAIN_NAME",
        default_value = "tron-mainnet"
    )]
    firehose_chain_name: String,

    /// Hub broadcast capacity. Sets the per-subscriber lag tolerance
    /// — a subscriber more than this many blocks behind gets an
    /// `RESOURCE_EXHAUSTED` status and must reconnect. 1024 covers
    /// roughly an hour of TRON blocks at 3s spacing.
    #[arg(long, env = "LIGHTCYCLE_RELAY_HUB_CAPACITY", default_value_t = 1024)]
    firehose_hub_capacity: usize,

    /// Whether to fetch the `TransactionInfo` side channel for each
    /// block (logs, internal txs, resource accounting). Default true:
    /// the dominant TRC-20 / TRC-721 indexing use case needs it. Set
    /// false to halve the per-tick RPC cost when consumers don't read
    /// `Transaction.info`. The relayer soft-degrades on tx-info RPC
    /// errors regardless (block ships with empty info, logged + counted),
    /// so this flag is for explicit-disable rather than fault-tolerance.
    #[arg(
        long,
        env = "LIGHTCYCLE_RELAY_FETCH_TX_INFO",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    fetch_tx_info: bool,
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
    lightcycle_store::describe_metrics();
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

    let initial_sr_set = if verify_policy != VerifyPolicy::Disabled {
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

    // Build the SR-set watch channel. The receiver lives inside the
    // service; the sender lives here so we can push refreshed sets
    // from the background task.
    let (sr_set_tx, sr_set_rx) =
        tokio::sync::watch::channel::<Option<lightcycle_types::SrSet>>(initial_sr_set);

    // Spawn the refresh loop only if (a) verify is on and (b) the
    // operator didn't explicitly disable refresh by passing 0.
    let sr_refresh_handle = if verify_policy != VerifyPolicy::Disabled && args.sr_refresh_secs > 0 {
        // We need a separate gRPC connection for the refresh path
        // because GrpcBlockFetcher takes ownership of the existing
        // source.
        let mut refresh_source = lightcycle_source::GrpcSource::connect(args.grpc_url.clone())
            .await
            .with_context(|| {
                format!("gRPC connect (refresh): {}", args.grpc_url)
            })?;
        let interval = std::time::Duration::from_secs(args.sr_refresh_secs);
        let tx = sr_set_tx.clone();
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately — skip it so we don't
            // re-fetch right after the startup fetch.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match refresh_source.fetch_active_sr_set().await {
                    Ok(set) => {
                        tracing::info!(size = set.len(), "SR set refreshed");
                        metrics::counter!(
                            "lightcycle_relay_sr_set_refreshes_total",
                            "result" => "ok"
                        )
                        .increment(1);
                        if tx.send(Some(set)).is_err() {
                            tracing::info!("SR-set channel closed; refresh task exiting");
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "SR set refresh failed; will retry");
                        metrics::counter!(
                            "lightcycle_relay_sr_set_refreshes_total",
                            "result" => "error"
                        )
                        .increment(1);
                    }
                }
            }
        }))
    } else {
        None
    };
    metrics::describe_counter!(
        "lightcycle_relay_sr_set_refreshes_total",
        "SR-set refresh attempts, labelled by result"
    );

    let engine = ReorgEngine::new(ReorgConfig {
        buffer_window: args.buffer_window,
        finality_depth: args.finality_depth,
    });
    let fetcher = GrpcBlockFetcher::new(source).with_fetch_tx_info(args.fetch_tx_info);
    let service = RelayerService::new(
        fetcher,
        engine,
        sr_set_rx,
        verify_policy,
        std::time::Duration::from_millis(args.poll_interval_ms.max(50)),
    );

    // mpsc from service to either the hub (firehose enabled) or a
    // direct drain (firehose disabled). 256 is a comfortable buffer
    // for the engine's burst pattern.
    let (output_tx, output_rx) = tokio::sync::mpsc::channel(256);
    let service_handle = tokio::spawn(service.run(output_tx));

    // Wire the firehose gRPC server when the operator asked for it.
    // The hub pump task drains output_rx → broadcast; the gRPC server
    // serves Stream.Blocks subscribers from the broadcast.
    let firehose_handles = if let Some(listen) = args.firehose_listen {
        lightcycle_firehose::describe_metrics();
        let hub = lightcycle_firehose::Hub::new(args.firehose_hub_capacity);
        let pump_handle = hub.pump_from(output_rx);

        // Build the Fetch.Block oracle. Needs its own GrpcSource
        // connection because the relayer's source is owned by the live-
        // tail pipeline (Fetch.Block fires on operator request, often
        // out of phase with the tick loop). Sharing the connection
        // would serialize Fetch behind the live-tail mutex and add
        // latency tail at no benefit. Costs one extra gRPC channel.
        let oracle_source =
            lightcycle_source::GrpcSource::connect(args.grpc_url.clone())
                .await
                .with_context(|| {
                    format!("gRPC connect (Fetch.Block oracle): {}", args.grpc_url)
                })?;
        let oracle: lightcycle_firehose::SharedBlockOracle =
            std::sync::Arc::new(GrpcBlockOracle::new(oracle_source));

        let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let chain_name = args.firehose_chain_name.clone();
        let hub_for_serve = hub.clone();
        let server_handle = tokio::spawn(async move {
            if let Err(e) = lightcycle_firehose::serve(
                listen,
                hub_for_serve,
                oracle,
                chain_name,
                async move {
                    let _ = server_shutdown_rx.await;
                },
            )
            .await
            {
                tracing::error!(error = %e, "firehose server exited with error");
            }
        });
        Some((server_handle, pump_handle, server_shutdown_tx))
    } else {
        // Log-only mode: drain the mpsc into the void so the service
        // doesn't back-pressure. The service already logs each block;
        // we don't double-log here.
        let drain_handle = tokio::spawn(async move {
            let mut rx = output_rx;
            while rx.recv().await.is_some() {}
        });
        // Encode None signal as the same shape so the shutdown branch
        // below treats both modes uniformly.
        Some((drain_handle, tokio::spawn(async {}), {
            let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
            tx
        }))
    };

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let _ = (&mut shutdown).await;
    tracing::info!("ctrl-c received, shutting down relay");

    if let Some((server_handle, pump_handle, server_shutdown_tx)) = firehose_handles {
        // server_shutdown_tx is a fresh oneshot in log-only mode (no
        // server to shut down), so signaling it is harmless. In
        // firehose mode it triggers tonic's serve_with_shutdown.
        let _ = server_shutdown_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server_handle).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), pump_handle).await;
    }
    if let Some(h) = sr_refresh_handle {
        h.abort();
    }
    drop(sr_set_tx); // drop the sender so any lingering refresh recv() exits
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), service_handle).await;
    Ok(())
}

/// Backs `Fetch.Block` with a dedicated `GrpcSource` connection.
///
/// Each fetch performs two upstream RPCs: `Wallet.GetBlockByNum` for the
/// block bytes, then `WalletSolidity.GetNowBlock` for a fresh solidified-
/// head snapshot used to stamp the response's finality envelope. Two
/// RPCs per request is intentional for v0.1: it keeps the oracle
/// stateless (no clock drift relative to a cached head, no shared state
/// with the relayer), at the cost of one extra round-trip per
/// `Fetch.Block` call. Acceptable because Fetch is per-request, not
/// per-block-tail. When `lightcycle-store` lands a block + head cache,
/// a cached impl will land alongside this one and the CLI will compose
/// (cache → upstream fallback) without changing the firehose surface.
///
/// Upstream failures are mapped to `Ok(None)` (NotFound) for cleanly-
/// rejected fetches (block doesn't exist yet, lite-fullnode mode
/// returning empty) and `Err(_)` for transport/decode errors. The
/// firehose service translates these to `Status::not_found` and
/// `Status::internal` respectively.
struct GrpcBlockOracle {
    source: tokio::sync::Mutex<GrpcSource>,
}

impl GrpcBlockOracle {
    fn new(source: GrpcSource) -> Self {
        Self {
            source: tokio::sync::Mutex::new(source),
        }
    }
}

#[async_trait::async_trait]
impl lightcycle_firehose::BlockOracle for GrpcBlockOracle {
    async fn fetch_block_by_number(
        &self,
        height: u64,
    ) -> anyhow::Result<Option<(lightcycle_relayer::BufferedBlock, lightcycle_types::BlockFinality)>>
    {
        let mut src = self.source.lock().await;

        let block_msg = match src.fetch_block(height).await {
            Ok(b) => b,
            Err(e) => {
                // Heuristic: java-tron returns an empty Block for "not yet
                // produced" rather than a gRPC NotFound. The decoder
                // catches that downstream — we just propagate the error
                // and let the service map it to internal. The truly
                // "block doesn't exist" case lands as Ok(empty block) →
                // codec rejects → Err here. Practical effect: the client
                // sees Status::internal with a clear message rather than
                // Status::not_found, which is honest.
                return Err(e.context("Fetch.Block: upstream get_block_by_num failed"));
            }
        };

        // Empty Block (lite-fullnode mode, or the height is past head):
        // upstream returns a Block with no header. Surface as NotFound.
        if block_msg.block_header.is_none() {
            return Ok(None);
        }

        let solidified_head = src.fetch_solidified_head().await.unwrap_or(None);
        drop(src);

        let decoded = lightcycle_codec::decode_block_message(&block_msg)
            .context("Fetch.Block: decode failed")?;
        let buffered = lightcycle_relayer::BufferedBlock {
            height: decoded.header.height,
            block_id: decoded.header.block_id,
            parent_id: decoded.header.parent_id,
            fork_id: 0,
            decoded,
            tx_infos: vec![],
        };
        // has_buffered_descendant=false: Fetch.Block has no buffer
        // awareness. The honest answer is Seen (or Finalized if the
        // block is at-or-below the chain's solidified head). Confirmed
        // is reserved for blocks whose buffered-descendant state is
        // known, which is a Stream.Blocks concern.
        let finality = lightcycle_types::BlockFinality::for_block(
            buffered.height,
            solidified_head,
            false,
        );
        Ok(Some((buffered, finality)))
    }
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
