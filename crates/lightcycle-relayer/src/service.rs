//! `RelayerService`: drives the live ingest pipeline.
//!
//! `source.fetch_now_block` → `codec::decode_block_message` →
//! `verify_witness_signature` (optional) → `ReorgEngine::accept` →
//! emit `Output` over an mpsc channel.
//!
//! The service is the orchestrator the CLI's `relay` subcommand wraps;
//! it's also the substrate the future firehose gRPC server multiplexes
//! over. Pure async; no HTTP/gRPC framing, no Prometheus exporter — the
//! caller stands those up.
//!
//! ## Source abstraction
//!
//! Production uses [`GrpcBlockFetcher`], a thin adapter over
//! `lightcycle_source::GrpcSource`. Tests inject a mock via the
//! [`BlockFetcher`] trait — the engine + service flow can be exercised
//! deterministically without an upstream.
//!
//! ## Verify policy
//!
//! TRON's dual-engine SR signing means ~25% of mainnet blocks are
//! signed with SM2 (see `project_tron_dual_engine_sigs` memory + the
//! codec ARCHITECTURE caveat). Our secp256k1 sigverify returns
//! `WitnessAddressMismatch` on those blocks. The relayer needs a
//! policy for what to do:
//!
//! - [`VerifyPolicy::Disabled`] — never call sigverify. Useful for
//!   fixture replay and for benchmarks where verification is
//!   measured separately.
//! - [`VerifyPolicy::Lenient`] (default) — verify; accept
//!   WitnessAddressMismatch as expected SM2-class output, log + count.
//!   Reject other verify errors (malformed sig, signer not in SR
//!   set) — those indicate either a peer-quality problem or a
//!   compromised SR.
//! - [`VerifyPolicy::Strict`] — reject any verify failure. Will
//!   correctly drop ~25% of mainnet blocks. Use only when SM2
//!   support lands, or when running against an ECKey-only testnet.
//!
//! ## Gap handling
//!
//! The poll-interval default is 1s; TRON produces blocks every ~3s,
//! so we have plenty of slack to catch each one. If we still miss a
//! block (network blip, a slow tick, or the upstream advanced two
//! blocks while we were processing the previous), the engine returns
//! [`ReorgError::ParentNotInBuffer`]. The service responds by
//! **skipping forward** to the new head — the gap blocks are lost.
//! This matches the operational reality of lite-fullnode endpoints
//! (per ADR 0012, our default) where `GetBlockByNum` is closed and
//! gap-fill via RPC isn't possible. Strict gap-fill mode lands when
//! we run against full-history endpoints.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use lightcycle_codec::{
    decode_block_message, decode_transaction_info_list_message, verify_witness_signature,
    CodecError, DecodedBlock, DecodedTxInfo,
};
use lightcycle_source::GrpcSource;
use lightcycle_store::ConsistencyHorizonObserver;
use lightcycle_types::{BlockHeight, SrSet};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::engine::{Output, ReorgEngine, ReorgError};

/// Policy for how to react to verify failures. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyPolicy {
    Disabled,
    Lenient,
    Strict,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self::Lenient
    }
}

/// What `BlockFetcher` returns: the decoded block plus the side-channel
/// `TransactionInfo` payload (logs, internal txs, resource accounting)
/// joined by tx-id at the firehose encoder layer. `tx_infos` is empty
/// when the fetcher is configured without tx-info fetching, when the
/// upstream returned no info (empty block), or when the tx-info RPC
/// failed and the fetcher chose to soft-degrade.
#[derive(Debug, Clone)]
pub struct FetchedBlock {
    pub decoded: DecodedBlock,
    pub tx_infos: Vec<DecodedTxInfo>,
}

/// Source abstraction the service consumes. Production wires this to
/// `GrpcSource` via [`GrpcBlockFetcher`]; tests provide a vec-backed
/// implementation.
#[async_trait]
pub trait BlockFetcher: Send {
    async fn fetch_now_block(&mut self) -> Result<FetchedBlock>;

    /// Fetch the chain's current solidified-head height. Default is
    /// `Ok(None)` so test fetchers can opt out without overriding —
    /// they end up with no chain-finality oracle, which is the right
    /// "no finality available" regime per the type's docs. Production
    /// fetcher overrides this to call
    /// `WalletSolidity.GetNowBlock`.
    async fn fetch_solidified_head(&mut self) -> Result<Option<BlockHeight>> {
        Ok(None)
    }
}

/// Production adapter: hits java-tron's `Wallet.GetNowBlock` and
/// (optionally) `Wallet.GetTransactionInfoByBlockNum`, decodes both
/// via the codec.
///
/// `fetch_tx_info` controls whether the second RPC fires. When false,
/// `FetchedBlock.tx_infos` is always empty. When true, a tx-info
/// RPC failure soft-degrades to empty `tx_infos` (logged + counted)
/// rather than failing the whole tick — losing logs is preferable
/// to halting the stream over a transient upstream issue.
#[derive(Debug)]
pub struct GrpcBlockFetcher {
    source: GrpcSource,
    fetch_tx_info: bool,
}

impl GrpcBlockFetcher {
    pub fn new(source: GrpcSource) -> Self {
        Self {
            source,
            fetch_tx_info: true,
        }
    }

    /// Override the default of fetching tx-info. The CLI exposes this
    /// as `--no-fetch-tx-info`.
    pub fn with_fetch_tx_info(mut self, fetch_tx_info: bool) -> Self {
        self.fetch_tx_info = fetch_tx_info;
        self
    }
}

#[async_trait]
impl BlockFetcher for GrpcBlockFetcher {
    async fn fetch_solidified_head(&mut self) -> Result<Option<BlockHeight>> {
        self.source.fetch_solidified_head().await
    }

    async fn fetch_now_block(&mut self) -> Result<FetchedBlock> {
        let block = self.source.fetch_now_block().await?;
        let decoded = decode_block_message(&block).context("codec decode failed")?;

        let tx_infos = if self.fetch_tx_info && !decoded.transactions.is_empty() {
            match self
                .source
                .fetch_transaction_info_by_block_num(decoded.header.height)
                .await
            {
                Ok(list) => match decode_transaction_info_list_message(&list) {
                    Ok(infos) => {
                        metrics::counter!(
                            "lightcycle_relay_tx_info_fetch_total",
                            "result" => "ok"
                        )
                        .increment(1);
                        infos
                    }
                    Err(e) => {
                        warn!(
                            height = decoded.header.height,
                            error = %e,
                            "tx-info decode failed; emitting block without info"
                        );
                        metrics::counter!(
                            "lightcycle_relay_tx_info_fetch_total",
                            "result" => "decode_error"
                        )
                        .increment(1);
                        Vec::new()
                    }
                },
                Err(e) => {
                    warn!(
                        height = decoded.header.height,
                        error = %e,
                        "tx-info RPC failed; emitting block without info"
                    );
                    metrics::counter!(
                        "lightcycle_relay_tx_info_fetch_total",
                        "result" => "rpc_error"
                    )
                    .increment(1);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        Ok(FetchedBlock { decoded, tx_infos })
    }
}

/// Service-level errors. The service does NOT bubble `ReorgError`
/// directly — it absorbs the recoverable variants (gap, duplicate)
/// internally and only surfaces fatal conditions.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("downstream output channel closed; consumer disconnected")]
    OutputChannelClosed,
    #[error("upstream source error: {0:#}")]
    SourceError(#[source] anyhow::Error),
    #[error("strict verify policy rejected a block: {0}")]
    VerifyRejected(#[source] CodecError),
}

/// Live ingest pipeline.
///
/// SR set lives behind a [`watch::Receiver`] so the CLI can refresh
/// it across TRON's maintenance-period transitions (every 7,200
/// blocks ≈ 6 hours) without restarting the service. See
/// [`static_sr_set`] for tests / log-only mode where no refresh task
/// exists.
pub struct RelayerService<F: BlockFetcher> {
    fetcher: F,
    engine: ReorgEngine,
    sr_set: watch::Receiver<Option<SrSet>>,
    verify_policy: VerifyPolicy,
    poll_interval: Duration,
    /// SLO observer (per ADR-0021): records seen→finalized latency
    /// into `lightcycle_store_block_seen_to_finalized_seconds`.
    /// Lives in the service rather than the engine because the
    /// engine is sync-pure; the observer carries `Instant` state and
    /// the metric emit is a service-layer concern.
    horizon: ConsistencyHorizonObserver,
}

impl<F: BlockFetcher> std::fmt::Debug for RelayerService<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayerService")
            .field("verify_policy", &self.verify_policy)
            .field("poll_interval", &self.poll_interval)
            .field("engine_len", &self.engine.len())
            .field(
                "sr_set_size",
                &self.sr_set.borrow().as_ref().map(|s| s.len()),
            )
            .finish()
    }
}

/// Build a static, non-refreshable SR-set channel for callers that
/// don't run a refresh task (tests, log-only mode). The `Sender` is
/// dropped immediately; `borrow()` on the receiver continues to
/// return the initial value forever.
pub fn static_sr_set(set: Option<SrSet>) -> watch::Receiver<Option<SrSet>> {
    let (tx, rx) = watch::channel(set);
    drop(tx);
    rx
}

impl<F: BlockFetcher> RelayerService<F> {
    pub fn new(
        fetcher: F,
        engine: ReorgEngine,
        sr_set: watch::Receiver<Option<SrSet>>,
        verify_policy: VerifyPolicy,
        poll_interval: Duration,
    ) -> Self {
        Self {
            fetcher,
            engine,
            sr_set,
            verify_policy,
            poll_interval,
            // 1024 covers TRON's reorg window with generous slack;
            // sustained orphan rate would evict before this saturates.
            horizon: ConsistencyHorizonObserver::new(1024),
        }
    }

    /// Run the pipeline forever (or until the output channel closes).
    /// Each tick: fetch → decode → verify → engine.accept → emit.
    pub async fn run(mut self, output_tx: mpsc::Sender<Output>) -> Result<(), ServiceError> {
        let mut ticker = interval(self.poll_interval);
        // If a tick fires while we're mid-fetch (slow round-trip or a
        // big block), skip the missed ticks — we only want the latest
        // head, not a backlog of polling intent.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        info!(
            poll_interval_ms = self.poll_interval.as_millis() as u64,
            verify_policy = ?self.verify_policy,
            sr_set_size = self.sr_set.borrow().as_ref().map(|s| s.len()).unwrap_or(0),
            "relayer pipeline starting"
        );

        loop {
            ticker.tick().await;
            if output_tx.is_closed() {
                info!("output channel closed; service exiting");
                return Ok(());
            }
            if let Err(e) = self.tick_once(&output_tx).await {
                match e {
                    ServiceError::OutputChannelClosed => return Ok(()),
                    ServiceError::SourceError(_) => {
                        // Transient: log + retry next tick.
                        warn!(error = %e, "source error; will retry");
                        metrics::counter!("lightcycle_relay_source_errors_total").increment(1);
                    }
                    ServiceError::VerifyRejected(_) => {
                        // Strict policy + the block didn't verify. Already
                        // counted in process_one; surface and move on.
                        warn!(error = %e, "block rejected by strict verify policy");
                    }
                }
            }
        }
    }

    async fn tick_once(&mut self, output_tx: &mpsc::Sender<Output>) -> Result<(), ServiceError> {
        // Refresh the chain-reported solidified head per ADR-0021's
        // "the chain has a finality machine; lean on it" — soft-fail
        // on RPC error so a transient solidity-RPC blip doesn't halt
        // the live stream. Counted via
        // `lightcycle_relay_solidified_head_fetch_total`. Any
        // Irreversible / ForkResolved outputs the head advance triggers
        // are forwarded on the same channel as block emissions.
        match self.fetcher.fetch_solidified_head().await {
            Ok(Some(h)) => {
                metrics::counter!(
                    "lightcycle_relay_solidified_head_fetch_total",
                    "result" => "ok"
                )
                .increment(1);
                metrics::gauge!("lightcycle_relay_solidified_head").set(h as f64);
                let outs = self.engine.set_solidified_head(h);
                for output in outs {
                    log_output(&output);
                    self.observe_for_horizon(&output);
                    metrics::counter!(
                        "lightcycle_relay_outputs_total",
                        "step" => step_label(&output)
                    )
                    .increment(1);
                    if output_tx.send(output).await.is_err() {
                        return Err(ServiceError::OutputChannelClosed);
                    }
                }
            }
            Ok(None) => {
                metrics::counter!(
                    "lightcycle_relay_solidified_head_fetch_total",
                    "result" => "empty"
                )
                .increment(1);
            }
            Err(e) => {
                warn!(error = %e, "solidified-head RPC failed; finality emission paused this tick");
                metrics::counter!(
                    "lightcycle_relay_solidified_head_fetch_total",
                    "result" => "rpc_error"
                )
                .increment(1);
            }
        }

        let fetched = self
            .fetcher
            .fetch_now_block()
            .await
            .map_err(ServiceError::SourceError)?;

        // De-dupe: if this is the same head we already accepted at the
        // tip, nothing to do. Cheaper than going through the engine
        // and coming back with a DuplicateBlock error.
        if let Some(tip) = self.engine.tip() {
            if tip.block_id == fetched.decoded.header.block_id {
                metrics::counter!("lightcycle_relay_polls_total", "result" => "duplicate")
                    .increment(1);
                return Ok(());
            }
        }

        self.process_one(fetched, output_tx).await
    }

    async fn process_one(
        &mut self,
        fetched: FetchedBlock,
        output_tx: &mpsc::Sender<Output>,
    ) -> Result<(), ServiceError> {
        let FetchedBlock { decoded, tx_infos } = fetched;
        let height = decoded.header.height;
        let block_id = decoded.header.block_id;

        // Verify if a policy + SR set is in place. Snapshot the SR
        // set out of the watch so we don't hold the read guard across
        // any await points in the engine path below (and also so that
        // if a refresh fires mid-tick, we're using the version we
        // started with).
        let sr_set_snapshot: Option<SrSet> = self.sr_set.borrow().clone();
        if self.verify_policy != VerifyPolicy::Disabled {
            if let Some(sr) = &sr_set_snapshot {
                match verify_witness_signature(&decoded.header, sr) {
                    Ok(()) => {
                        metrics::counter!("lightcycle_relay_verify_total", "result" => "ok")
                            .increment(1);
                    }
                    Err(e)
                        if matches!(e, CodecError::WitnessAddressMismatch { .. })
                            && self.verify_policy == VerifyPolicy::Lenient =>
                    {
                        // Expected SM2-class block. Accept under
                        // lenient policy; log + count.
                        debug!(
                            height,
                            block_id = %hex::encode(block_id.0),
                            "SM2-class block accepted under lenient verify policy"
                        );
                        metrics::counter!(
                            "lightcycle_relay_verify_total",
                            "result" => "sm2_lenient"
                        )
                        .increment(1);
                    }
                    Err(e) => {
                        metrics::counter!(
                            "lightcycle_relay_verify_total",
                            "result" => "rejected"
                        )
                        .increment(1);
                        if self.verify_policy == VerifyPolicy::Strict {
                            return Err(ServiceError::VerifyRejected(e));
                        }
                        // Lenient + non-SM2 failure: malformed sig or
                        // signer not in SR set. Log loudly but don't
                        // halt — peer-quality issue, expected to be
                        // rare, recover on next tick.
                        warn!(
                            height,
                            block_id = %hex::encode(block_id.0),
                            error = %e,
                            "verify failed under lenient policy; skipping block"
                        );
                        return Ok(());
                    }
                }
            } else {
                // Verify policy is on but no SR set provided. Treat as
                // a config bug — log once per occurrence (counter
                // catches the rate).
                metrics::counter!(
                    "lightcycle_relay_verify_total",
                    "result" => "no_sr_set"
                )
                .increment(1);
                debug!(
                    "verify policy enabled but no SR set; pass-through. \
                     Caller should fetch_active_sr_set + update_sr_set."
                );
            }
        }

        // Feed the engine.
        match self.engine.accept(decoded, tx_infos) {
            Ok(outputs) => {
                for output in outputs {
                    log_output(&output);
                    self.observe_for_horizon(&output);
                    metrics::counter!(
                        "lightcycle_relay_outputs_total",
                        "step" => step_label(&output)
                    )
                    .increment(1);
                    if output_tx.send(output).await.is_err() {
                        return Err(ServiceError::OutputChannelClosed);
                    }
                }
                metrics::counter!("lightcycle_relay_polls_total", "result" => "accepted")
                    .increment(1);
                metrics::gauge!("lightcycle_relay_engine_buffer_depth")
                    .set(self.engine.len() as f64);
                Ok(())
            }
            Err(ReorgError::ParentNotInBuffer { .. }) => {
                // Gap detected. SkipForward: log + count, leave engine
                // state untouched, next tick will see whatever the new
                // head is. If the new head is canonically connected to
                // our tip we recover; if not we'll keep skipping until
                // operator intervenes.
                warn!(
                    height,
                    block_id = %hex::encode(block_id.0),
                    "gap: parent not in buffer; skip-forward (lite-fullnode default)"
                );
                metrics::counter!("lightcycle_relay_polls_total", "result" => "gap_skipped")
                    .increment(1);
                Ok(())
            }
            Err(ReorgError::ReorgBelowFinality { .. }) => {
                // Upstream is delivering a block that would orphan a
                // finalized one. Symptom of an upstream node issue or a
                // truly catastrophic chain reorg. Log error, continue —
                // crashing here would just compound the operational
                // problem.
                error!(
                    height,
                    block_id = %hex::encode(block_id.0),
                    "reorg below finality refused by engine"
                );
                metrics::counter!(
                    "lightcycle_relay_polls_total",
                    "result" => "reorg_below_finality"
                )
                .increment(1);
                Ok(())
            }
            Err(ReorgError::DuplicateBlock { .. }) => {
                metrics::counter!("lightcycle_relay_polls_total", "result" => "duplicate")
                    .increment(1);
                Ok(())
            }
        }
    }

    /// Test/debug surface: read the current engine tip (immutable).
    pub fn tip_height(&self) -> Option<lightcycle_types::BlockHeight> {
        self.engine.tip().map(|t| t.height)
    }

    /// Feed the consistency-horizon observer (ADR-0021 SLO). Called
    /// for every output before it ships; records seen→finalized
    /// latency into
    /// `lightcycle_store_block_seen_to_finalized_seconds`.
    /// Pass-through for variants that don't have a single block id
    /// (ledger entries) or aren't tier transitions (Undo).
    fn observe_for_horizon(&mut self, output: &Output) {
        match output {
            Output::New(s) => {
                self.horizon.observe_seen(s.block.block_id, s.block.height);
            }
            Output::Irreversible(s) => {
                self.horizon.observe_finalized(s.block.block_id);
            }
            // Undo: orphaned block won't reach Finalized via this id;
            // observer eviction (max_pending) cleans it up eventually.
            // ForkObserved / ForkResolved: ledger-only, no per-id
            // tier transition to record.
            Output::Undo(_) | Output::ForkObserved { .. } | Output::ForkResolved { .. } => {}
        }
    }
}

fn step_label(o: &Output) -> &'static str {
    match o {
        Output::New(_) => "new",
        Output::Undo(_) => "undo",
        Output::Irreversible(_) => "irreversible",
        Output::ForkObserved { .. } => "fork_observed",
        Output::ForkResolved { .. } => "fork_resolved",
    }
}

fn log_output(o: &Output) {
    match o {
        Output::New(s) => info!(
            step = "NEW",
            height = s.block.height,
            block_id = %hex::encode(s.block.block_id.0),
            tx_count = s.block.decoded.transactions.len(),
            tier = ?s.finality.tier,
            solidified_head = s.finality.solidified_head,
            "block emitted"
        ),
        Output::Undo(s) => warn!(
            step = "UNDO",
            height = s.block.height,
            block_id = %hex::encode(s.block.block_id.0),
            tier = ?s.finality.tier,
            "block orphaned by reorg"
        ),
        Output::Irreversible(s) => info!(
            step = "IRREVERSIBLE",
            height = s.block.height,
            block_id = %hex::encode(s.block.block_id.0),
            solidified_head = s.finality.solidified_head,
            "block crossed finality (chain-reported)"
        ),
        Output::ForkObserved {
            observed_at_height,
            kept_tip,
            orphaned_tips,
        } => warn!(
            step = "FORK_OBSERVED",
            height = observed_at_height,
            kept_tip = %hex::encode(kept_tip.0),
            orphan_count = orphaned_tips.len(),
            "fork observed; awaiting chain finality to resolve"
        ),
        Output::ForkResolved {
            observed_at_height,
            finalized_head,
            finalized_tip,
            orphaned_tips,
        } => info!(
            step = "FORK_RESOLVED",
            observed_at_height = observed_at_height,
            finalized_head = finalized_head,
            finalized_tip = %hex::encode(finalized_tip.0),
            orphan_count = orphaned_tips.len(),
            "chain finalized past fork; resolution logged"
        ),
    }
}

/// Describe service-level metrics so they appear in Prometheus output
/// even before the first observation. Mirrors `rpc::describe_metrics`.
pub fn describe_metrics() {
    metrics::describe_counter!(
        "lightcycle_relay_polls_total",
        "polls of the upstream block source, labelled by result"
    );
    metrics::describe_counter!(
        "lightcycle_relay_source_errors_total",
        "source-layer errors (RPC failures, decode failures)"
    );
    metrics::describe_counter!(
        "lightcycle_relay_verify_total",
        "witness-signature verifications, labelled by result"
    );
    metrics::describe_counter!(
        "lightcycle_relay_outputs_total",
        "blocks emitted to consumers, labelled by step (new|undo|irreversible)"
    );
    metrics::describe_gauge!(
        "lightcycle_relay_engine_buffer_depth",
        "current canonical-buffer depth in the reorg engine"
    );
    metrics::describe_counter!(
        "lightcycle_relay_tx_info_fetch_total",
        "tx-info side-channel fetches, labelled by result (ok|rpc_error|decode_error)"
    );
    metrics::describe_counter!(
        "lightcycle_relay_solidified_head_fetch_total",
        "solidified-head fetches from WalletSolidity.GetNowBlock, by result (ok|empty|rpc_error)"
    );
    metrics::describe_gauge!(
        "lightcycle_relay_solidified_head",
        "chain-reported solidified-head block number (per ADR-0021 the only legal finality oracle)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ReorgConfig;
    use lightcycle_codec::DecodedHeader;
    use lightcycle_types::{Address, BlockId};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Vec-backed mock fetcher: returns blocks in the order they were
    /// pushed, then errors on `fetch_now_block` once exhausted.
    /// Implements `Clone` via Arc<Mutex<...>> so we can hand the same
    /// queue to a test harness and the service. Tests that don't need
    /// tx_infos can use [`Self::new`] which defaults them to empty;
    /// tests covering the join path use [`Self::new_with_infos`].
    #[derive(Clone)]
    struct ScriptedFetcher {
        queue: Arc<Mutex<Vec<FetchedBlock>>>,
    }

    impl ScriptedFetcher {
        fn new(blocks: Vec<DecodedBlock>) -> Self {
            let fetched = blocks
                .into_iter()
                .map(|decoded| FetchedBlock {
                    decoded,
                    tx_infos: Vec::new(),
                })
                .collect();
            Self {
                queue: Arc::new(Mutex::new(fetched)),
            }
        }
    }

    #[async_trait]
    impl BlockFetcher for ScriptedFetcher {
        async fn fetch_now_block(&mut self) -> Result<FetchedBlock> {
            let mut q = self.queue.lock().await;
            if q.is_empty() {
                anyhow::bail!("scripted queue exhausted");
            }
            Ok(q.remove(0))
        }
    }

    fn synth_block(height: u64, block_id: [u8; 32], parent_id: [u8; 32]) -> DecodedBlock {
        DecodedBlock {
            header: DecodedHeader {
                height,
                block_id: BlockId(block_id),
                parent_id: BlockId(parent_id),
                raw_data_hash: [0u8; 32],
                tx_trie_root: [0u8; 32],
                timestamp_ms: 1_777_854_558_000,
                witness: Address([0x41; 21]),
                witness_signature: vec![0u8; 65],
                version: 34,
            },
            transactions: vec![],
        }
    }

    fn id(height: u64, tag: u8) -> [u8; 32] {
        let mut a = [tag; 32];
        a[..8].copy_from_slice(&height.to_be_bytes());
        a
    }

    fn small_engine() -> ReorgEngine {
        ReorgEngine::new(ReorgConfig {
            buffer_window: 5,
            finality_depth: 3,
        })
    }

    /// Drain up to `n` outputs (or fewer, if the channel is empty +
    /// the producer has hung up). Used to assert ordered emission.
    async fn drain(rx: &mut mpsc::Receiver<Output>, n: usize) -> Vec<Output> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(o)) => out.push(o),
                _ => break,
            }
        }
        out
    }

    #[tokio::test(flavor = "current_thread")]
    async fn linear_flow_emits_new_per_block() {
        let blocks = (100..104)
            .scan(id(99, 0), |prev, h| {
                let cur = id(h, 0xa);
                let b = synth_block(h, cur, *prev);
                *prev = cur;
                Some(b)
            })
            .collect::<Vec<_>>();
        let fetcher = ScriptedFetcher::new(blocks);
        let svc = RelayerService::new(
            fetcher,
            small_engine(),
            static_sr_set(None),
            VerifyPolicy::Disabled,
            Duration::from_millis(10),
        );

        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(svc.run(tx));

        // Allow the timer-paused harness to advance until the queue
        // drains. The service exits with SourceError once the
        // scripted queue is empty (no more blocks to fetch); we drop
        // its receiver to bail cleanly.
        let outputs = drain(&mut rx, 4).await;
        drop(rx);
        // Service may still be looping on the timer trying to fetch;
        // give it a moment to notice the channel closed.
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

        let new_heights: Vec<_> = outputs
            .iter()
            .filter_map(|o| match o {
                Output::New(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(new_heights, vec![100, 101, 102, 103]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reorg_flow_emits_undo_then_new() {
        // 100a, 101a, 102a, then 102b reorg.
        let mut prev = id(99, 0);
        let mut blocks = Vec::new();
        for h in 100..103 {
            let cur = id(h, 0xa);
            blocks.push(synth_block(h, cur, prev));
            prev = cur;
        }
        // Reorg block: 102b with parent = 101a.
        blocks.push(synth_block(102, id(102, 0xb), id(101, 0xa)));

        let fetcher = ScriptedFetcher::new(blocks);
        let svc = RelayerService::new(
            fetcher,
            small_engine(),
            static_sr_set(None),
            VerifyPolicy::Disabled,
            Duration::from_millis(10),
        );

        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(svc.run(tx));

        let outputs = drain(&mut rx, 5).await; // 3 NEW + UNDO + NEW
        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

        let kinds: Vec<_> = outputs
            .iter()
            .filter_map(|o| match o {
                Output::New(s) => Some(("new", s.block.height, s.block.block_id.0[8])),
                Output::Undo(s) => Some(("undo", s.block.height, s.block.block_id.0[8])),
                Output::Irreversible(s) => Some(("irrev", s.block.height, s.block.block_id.0[8])),
                // Ledger-entry variants don't have a single block_id;
                // they're tested separately.
                Output::ForkObserved { .. } | Output::ForkResolved { .. } => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("new", 100, 0xa),
                ("new", 101, 0xa),
                ("new", 102, 0xa),
                ("undo", 102, 0xa),
                ("new", 102, 0xb),
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gap_triggers_skip_forward_no_panic() {
        // 100a, then 102a (skipping 101a) — engine returns ParentNotInBuffer.
        // Service should swallow it and continue (no further blocks here so
        // it'll exit on source exhaustion).
        let blocks = vec![
            synth_block(100, id(100, 0xa), id(99, 0)),
            synth_block(102, id(102, 0xa), id(101, 0xa)),
        ];
        let fetcher = ScriptedFetcher::new(blocks);
        let svc = RelayerService::new(
            fetcher,
            small_engine(),
            static_sr_set(None),
            VerifyPolicy::Disabled,
            Duration::from_millis(10),
        );

        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(svc.run(tx));

        let outputs = drain(&mut rx, 2).await;
        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

        // Only block 100a should have made it through; 102a's gap
        // gets skipped silently.
        let new_heights: Vec<_> = outputs
            .iter()
            .filter_map(|o| match o {
                Output::New(s) => Some(s.block.height),
                _ => None,
            })
            .collect();
        assert_eq!(new_heights, vec![100]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_head_is_silently_ignored() {
        // The same head delivered twice — common when poll faster than
        // block production. Only one NEW should fire.
        let block = synth_block(100, id(100, 0xa), id(99, 0));
        let blocks = vec![block.clone(), block];
        let fetcher = ScriptedFetcher::new(blocks);
        let svc = RelayerService::new(
            fetcher,
            small_engine(),
            static_sr_set(None),
            VerifyPolicy::Disabled,
            Duration::from_millis(10),
        );

        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(svc.run(tx));

        let outputs = drain(&mut rx, 2).await;
        drop(rx);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

        let new_count = outputs
            .iter()
            .filter(|o| matches!(o, Output::New(_)))
            .count();
        assert_eq!(new_count, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_channel_close_shuts_service_down() {
        let blocks = vec![synth_block(100, id(100, 0xa), id(99, 0))];
        let fetcher = ScriptedFetcher::new(blocks);
        let svc = RelayerService::new(
            fetcher,
            small_engine(),
            static_sr_set(None),
            VerifyPolicy::Disabled,
            Duration::from_millis(10),
        );

        let (tx, rx) = mpsc::channel(16);
        let handle = tokio::spawn(svc.run(tx));
        // Drop the receiver immediately. Service should exit cleanly
        // on the next tick (rather than panic or loop forever).
        drop(rx);

        let r = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("service should exit when output channel closes");
        assert!(matches!(r, Ok(Ok(()))));
    }
}
