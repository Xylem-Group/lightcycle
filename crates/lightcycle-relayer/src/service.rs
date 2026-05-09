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
use lightcycle_codec::{decode_block_message, verify_witness_signature, CodecError, DecodedBlock};
use lightcycle_source::GrpcSource;
use lightcycle_types::SrSet;
use thiserror::Error;
use tokio::sync::mpsc;
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

/// Source abstraction the service consumes. Production wires this to
/// `GrpcSource` via [`GrpcBlockFetcher`]; tests provide a vec-backed
/// implementation.
#[async_trait]
pub trait BlockFetcher: Send {
    async fn fetch_now_block(&mut self) -> Result<DecodedBlock>;
}

/// Production adapter: hits java-tron's `Wallet.GetNowBlock`, decodes
/// via the codec.
#[derive(Debug)]
pub struct GrpcBlockFetcher {
    source: GrpcSource,
}

impl GrpcBlockFetcher {
    pub fn new(source: GrpcSource) -> Self {
        Self { source }
    }
}

#[async_trait]
impl BlockFetcher for GrpcBlockFetcher {
    async fn fetch_now_block(&mut self) -> Result<DecodedBlock> {
        let block = self.source.fetch_now_block().await?;
        Ok(decode_block_message(&block).context("codec decode failed")?)
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
pub struct RelayerService<F: BlockFetcher> {
    fetcher: F,
    engine: ReorgEngine,
    sr_set: Option<SrSet>,
    verify_policy: VerifyPolicy,
    poll_interval: Duration,
}

impl<F: BlockFetcher> std::fmt::Debug for RelayerService<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayerService")
            .field("verify_policy", &self.verify_policy)
            .field("poll_interval", &self.poll_interval)
            .field("engine_len", &self.engine.len())
            .field("sr_set_size", &self.sr_set.as_ref().map(|s| s.len()))
            .finish()
    }
}

impl<F: BlockFetcher> RelayerService<F> {
    pub fn new(
        fetcher: F,
        engine: ReorgEngine,
        sr_set: Option<SrSet>,
        verify_policy: VerifyPolicy,
        poll_interval: Duration,
    ) -> Self {
        Self {
            fetcher,
            engine,
            sr_set,
            verify_policy,
            poll_interval,
        }
    }

    /// Replace the SR set live. Driver code can refresh on epoch
    /// transitions (every 7,200 blocks ~ 6h) by calling this.
    pub fn update_sr_set(&mut self, sr_set: SrSet) {
        self.sr_set = Some(sr_set);
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
            sr_set_size = self.sr_set.as_ref().map(|s| s.len()).unwrap_or(0),
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
        let decoded = self
            .fetcher
            .fetch_now_block()
            .await
            .map_err(ServiceError::SourceError)?;

        // De-dupe: if this is the same head we already accepted at the
        // tip, nothing to do. Cheaper than going through the engine
        // and coming back with a DuplicateBlock error.
        if let Some(tip) = self.engine.tip() {
            if tip.block_id == decoded.header.block_id {
                metrics::counter!("lightcycle_relay_polls_total", "result" => "duplicate")
                    .increment(1);
                return Ok(());
            }
        }

        self.process_one(decoded, output_tx).await
    }

    async fn process_one(
        &mut self,
        decoded: DecodedBlock,
        output_tx: &mpsc::Sender<Output>,
    ) -> Result<(), ServiceError> {
        let height = decoded.header.height;
        let block_id = decoded.header.block_id;

        // Verify if a policy + SR set is in place.
        if self.verify_policy != VerifyPolicy::Disabled {
            if let Some(sr) = &self.sr_set {
                match verify_witness_signature(&decoded.header, sr) {
                    Ok(()) => {
                        metrics::counter!("lightcycle_relay_verify_total", "result" => "ok")
                            .increment(1);
                    }
                    Err(e) if matches!(e, CodecError::WitnessAddressMismatch { .. })
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
        match self.engine.accept(decoded) {
            Ok(outputs) => {
                for output in outputs {
                    log_output(&output);
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
}

fn step_label(o: &Output) -> &'static str {
    match o {
        Output::New(_) => "new",
        Output::Undo(_) => "undo",
        Output::Irreversible(_) => "irreversible",
    }
}

fn log_output(o: &Output) {
    match o {
        Output::New(s) => info!(
            step = "NEW",
            height = s.block.height,
            block_id = %hex::encode(s.block.block_id.0),
            tx_count = s.block.decoded.transactions.len(),
            "block emitted"
        ),
        Output::Undo(s) => warn!(
            step = "UNDO",
            height = s.block.height,
            block_id = %hex::encode(s.block.block_id.0),
            "block orphaned by reorg"
        ),
        Output::Irreversible(s) => info!(
            step = "IRREVERSIBLE",
            height = s.block.height,
            block_id = %hex::encode(s.block.block_id.0),
            "block crossed finality"
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
    /// queue to a test harness and the service.
    #[derive(Clone)]
    struct ScriptedFetcher {
        queue: Arc<Mutex<Vec<DecodedBlock>>>,
    }

    impl ScriptedFetcher {
        fn new(blocks: Vec<DecodedBlock>) -> Self {
            Self {
                queue: Arc::new(Mutex::new(blocks)),
            }
        }
    }

    #[async_trait]
    impl BlockFetcher for ScriptedFetcher {
        async fn fetch_now_block(&mut self) -> Result<DecodedBlock> {
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
            None,
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
            None,
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
            .map(|o| match o {
                Output::New(s) => ("new", s.block.height, s.block.block_id.0[8]),
                Output::Undo(s) => ("undo", s.block.height, s.block.block_id.0[8]),
                Output::Irreversible(s) => ("irrev", s.block.height, s.block.block_id.0[8]),
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
            None,
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
            None,
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
            None,
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
