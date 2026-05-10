//! Firehose v2 gRPC server: `Stream.Blocks` + `Fetch.Block` + `EndpointInfo.Info`.
//!
//! Surface:
//!
//! - **`Stream.Blocks`** — live tail with optional in-cache backfill.
//!   When the StreamService has a [`SharedBlockCache`] attached
//!   (see [`StreamService::with_block_cache`]), requests with
//!   `start_block_num != 0` or a non-empty `cursor` walk the cache
//!   from the requested height to its tip, then transition into the
//!   live broadcast (with dedup so blocks emitted from the cache
//!   aren't re-emitted from live). Without a cache, requests with
//!   start hints get `FailedPrecondition`; live-tail requests work
//!   either way.
//! - **`Fetch.Block`** — point-in-time block lookup by height. Backed
//!   by a [`BlockOracle`](crate::oracle::BlockOracle) (the CLI wires
//!   a [`CachingBlockOracle`](crate::oracle::CachingBlockOracle) over
//!   the read-through cache + a `GrpcBlockOracle` fallback). Only
//!   `BlockNumber` references are supported in v0.1;
//!   `BlockHashAndNumber` and `Cursor` references return
//!   `FailedPrecondition`. No transforms.
//! - **`EndpointInfo.Info`** — minimal: chain name, BlockIdEncoding=HEX,
//!   first_streamable left at 0/empty (we don't track upstream chain
//!   genesis at v0.1).
//!
//! ## Response shape
//!
//! `Response.metadata` is fully populated: num, id (hex), parent_num,
//! parent_id (hex), lib_num, time. `Response.block` carries an `Any`
//! whose `type_url` is `sf.tron.type.v1.Block` and whose `value` is the
//! prost-encoded block — header, transactions, contracts (typed
//! payloads for the four high-volume contract kinds + raw bytes for
//! everything else), and `Transaction.info` (logs, internal txs,
//! resource accounting) joined from java-tron's
//! `getTransactionInfoByBlockNum` side channel.

use std::pin::Pin;

use lightcycle_proto::firehose::v2::{
    endpoint_info_server::EndpointInfo, fetch_server::Fetch as FetchSvc,
    info_response::BlockIdEncoding, single_block_request::Reference,
    stream_server::Stream as StreamSvc, BlockMetadata, ForkStep, InfoRequest, InfoResponse,
    Request, Response, SingleBlockRequest, SingleBlockResponse,
};
use lightcycle_relayer::{BufferedBlock, Cursor, Output, StreamableBlock};
use lightcycle_store::{BlockArchive, SharedBlockCache};
use lightcycle_types::{BlockFinality, BlockHeight, Step};
use prost::Message;
use prost_types::{Any, Timestamp};
use tokio::sync::watch;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::{Stream, StreamExt};
use tonic::Status;
use tracing::{debug, warn};

use crate::encode::{encode_block, BLOCK_TYPE_URL};
use crate::hub::Hub;
use crate::oracle::SharedBlockOracle;

/// Stream service. Holds a clone of the hub so each `blocks` RPC
/// call subscribes to the live broadcast.
///
/// When [`Self::with_block_cache`] has been called, the service also
/// honors `start_block_num` for in-cache backfill: it walks the
/// cache from the requested height up to whatever's available, then
/// transitions seamlessly into the live broadcast (with dedup so a
/// block emitted from the cache isn't re-emitted from live).
/// `start_block_num` requests with no cache attached, or with a
/// height outside the cache window, are rejected with a precise
/// `FailedPrecondition` message.
#[derive(Clone, Debug)]
pub struct StreamService {
    hub: Hub,
    block_cache: Option<SharedBlockCache<BufferedBlock>>,
    /// Solidified-head watch — needed to compute the right
    /// `BlockFinality` for cached (cursor-resumed) blocks at emit
    /// time. Same channel the relayer broadcasts to.
    solidified_head: Option<watch::Receiver<Option<BlockHeight>>>,
    /// Persistent block archive. Backfill walker first walks the
    /// archive (heights below `cache.min_height`) before chaining into
    /// the cache walk. `None` keeps lightcycle on the in-memory-only
    /// behavior; `Some(_)` extends backfill to the archive's retention
    /// floor.
    archive: Option<BlockArchive>,
}

impl StreamService {
    pub fn new(hub: Hub) -> Self {
        Self {
            hub,
            block_cache: None,
            solidified_head: None,
            archive: None,
        }
    }

    /// Attach the read-through block cache for in-cache backfill via
    /// `start_block_num`. Without this attach, requests with
    /// `start_block_num != 0` fall through to the v0.1 reject path.
    pub fn with_block_cache(
        mut self,
        cache: SharedBlockCache<BufferedBlock>,
        solidified_head: watch::Receiver<Option<BlockHeight>>,
    ) -> Self {
        self.block_cache = Some(cache);
        self.solidified_head = Some(solidified_head);
        self
    }

    /// Attach a persistent block archive. Stream.Blocks backfill will
    /// walk the archive when the consumer's resume height is below the
    /// in-memory cache window, extending resume capability past the
    /// in-memory horizon.
    pub fn with_archive(mut self, archive: BlockArchive) -> Self {
        self.archive = Some(archive);
        self
    }
}

#[tonic::async_trait]
impl StreamSvc for StreamService {
    type BlocksStream = Pin<Box<dyn Stream<Item = Result<Response, Status>> + Send + 'static>>;

    async fn blocks(
        &self,
        request: tonic::Request<Request>,
    ) -> Result<tonic::Response<Self::BlocksStream>, Status> {
        let req = request.into_inner();

        // Decide where to start. Cursor takes precedence over
        // start_block_num if both are set (a cursor encodes a
        // canonical position; start_block_num is just the height).
        let start_height: Option<BlockHeight> = if !req.cursor.is_empty() {
            let bytes = hex::decode(&req.cursor)
                .map_err(|e| Status::invalid_argument(format!("cursor is not valid hex: {e}")))?;
            let cur = Cursor::from_bytes(&bytes).ok_or_else(|| {
                Status::invalid_argument("cursor decode failed: expected 40-byte cursor blob")
            })?;
            // Resume from the next height; the consumer has already
            // seen `cur.height`.
            Some(cur.height.saturating_add(1))
        } else if req.start_block_num > 0 {
            // Firehose v2's start_block_num is i64; negative values
            // mean "head minus N" by convention but we don't honor that
            // in v0.1 (would require knowing live head at request
            // time). Treat 0 as "live tail only" (the default).
            Some(req.start_block_num as u64)
        } else if req.start_block_num < 0 {
            return Err(Status::failed_precondition(
                "negative start_block_num (head-relative offset) not implemented in v0.1; \
                 pass an absolute height",
            ));
        } else {
            None
        };

        if req.stop_block_num != 0 {
            return Err(Status::failed_precondition(
                "stop_block_num not implemented in v0.1; stream is unbounded",
            ));
        }
        if req.final_blocks_only {
            // Doable in v0.1 (filter on Output::Irreversible) but not
            // covered by tests yet; reject explicitly.
            return Err(Status::failed_precondition(
                "final_blocks_only not implemented in v0.1",
            ));
        }
        if !req.transforms.is_empty() {
            return Err(Status::failed_precondition(
                "transforms not implemented in v0.1",
            ));
        }

        debug!(?start_height, "new firehose subscriber connecting");
        metrics::counter!("lightcycle_firehose_subscriptions_total").increment(1);

        // Subscribe to live BEFORE walking the cache so we don't
        // miss anything emitted during the walk. The order here is
        // load-bearing for at-least-once delivery.
        let live_rx = self.hub.subscribe();

        // Decide on backfill mode.
        let backfill: Vec<Response> = if let Some(h) = start_height {
            let Some(cache) = &self.block_cache else {
                return Err(Status::failed_precondition(
                    "backfill requested (start_block_num or cursor) but no block cache \
                     attached to this firehose; restart relayer with --firehose-listen \
                     and --block-cache-capacity > 0",
                ));
            };
            let head = self.solidified_head.as_ref().and_then(|w| *w.borrow());

            // Snapshot the cache range. We don't refuse-on-empty here
            // anymore — when an archive is attached, an empty cache is
            // recoverable as long as the archive covers the request.
            let cache_window: Option<(BlockHeight, BlockHeight)> = {
                let guard = cache.read().await;
                match (guard.min_height(), guard.max_height()) {
                    (Some(mn), Some(mx)) => Some((mn, mx)),
                    _ => None,
                }
            };

            // Stage 1: walk the archive for any heights below the
            // cache's lower bound (or the entire request, if the cache
            // is empty). The archive holds pre-encoded `pb::Block`
            // bytes; we wrap them in a Response without decoding the
            // full block — only enough to read metadata fields.
            let archive_rows: Vec<Response> = match (&self.archive, cache_window) {
                (Some(arc), Some((min_h, _))) if h < min_h => {
                    archive_walk(arc, h, min_h.saturating_sub(1)).map_err(|e| {
                        Status::failed_precondition(format!(
                            "archive walk failed for [{h}, {}]: {e}",
                            min_h.saturating_sub(1)
                        ))
                    })?
                }
                (Some(arc), None) => {
                    // Cache is empty — try to serve entirely from the
                    // archive. Bound the walk by the archive's max so
                    // we don't ask for an impossible range.
                    let max = arc
                        .max_height()
                        .map_err(|e| Status::internal(format!("archive max_height: {e}")))?;
                    match max {
                        Some(top) if top >= h => archive_walk(arc, h, top).map_err(|e| {
                            Status::failed_precondition(format!(
                                "archive walk failed for [{h}, {top}]: {e}"
                            ))
                        })?,
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            };

            // After the archive walk, decide where the cache walk
            // resumes from. If the archive walked nothing, cache walk
            // starts at h. If the archive walked some heights, cache
            // walk starts at archive's last_emitted + 1 (which equals
            // cache.min_height when both are present).
            let cache_walk_from: BlockHeight = archive_rows
                .last()
                .and_then(|r| r.metadata.as_ref().map(|m| m.num.saturating_add(1)))
                .unwrap_or(h);

            // Stage 2: walk the cache from `cache_walk_from` to its
            // tip. Reject up front if the requested range falls
            // entirely outside what cache + archive can cover.
            let cache_rows: Vec<BufferedBlock> = match cache_window {
                Some((min_h, max_h)) => {
                    if cache_walk_from < min_h {
                        // Should not happen given the staging above,
                        // but guard against off-by-one bugs (e.g. an
                        // empty archive that didn't advance the
                        // cursor).
                        return Err(Status::failed_precondition(format!(
                            "start height {h} is below archive+cache coverage \
                             [archive=?, cache_min={min_h}, cache_max={max_h}]"
                        )));
                    }
                    if cache_walk_from > max_h.saturating_add(1) {
                        return Err(Status::failed_precondition(format!(
                            "start height {h} is ahead of cache tip {max_h}; \
                             remove start_block_num or wait for the chain to advance"
                        )));
                    }
                    let guard = cache.read().await;
                    let mut rows = Vec::new();
                    for height in cache_walk_from..=max_h {
                        if let Some((_id, buffered)) = guard.get_by_height(height) {
                            rows.push(buffered);
                        } else {
                            // Height is in the [min, max] interval but
                            // the entry was evicted between min/max
                            // snapshot and the per-height lookup.
                            // Treat as cache miss and refuse — this
                            // should be rare, and a partial walk would
                            // create gaps the consumer can't detect.
                            return Err(Status::failed_precondition(format!(
                                "block at height {height} evicted from cache mid-walk; \
                                 retry with the same start_block_num"
                            )));
                        }
                    }
                    rows
                }
                None => {
                    // Cache is empty. archive_rows may have served the
                    // entire request, or partially. If the archive
                    // didn't reach the live tip, the consumer would
                    // see a gap on the live side; reject unless the
                    // archive is empty and we're effectively a
                    // live-tail request from h.
                    if archive_rows.is_empty() && self.archive.is_some() {
                        return Err(Status::failed_precondition(
                            "block cache is empty and archive does not cover this request; \
                             reconnect after the relayer warms up",
                        ));
                    }
                    if archive_rows.is_empty() {
                        return Err(Status::failed_precondition(
                            "block cache is empty; cannot serve backfill yet",
                        ));
                    }
                    Vec::new()
                }
            };

            metrics::counter!(
                "lightcycle_firehose_backfill_total",
                "result" => "ok"
            )
            .increment(1);
            metrics::histogram!("lightcycle_firehose_backfill_blocks")
                .record((archive_rows.len() + cache_rows.len()) as f64);
            if !archive_rows.is_empty() {
                metrics::counter!("lightcycle_firehose_archive_backfill_blocks_total")
                    .increment(archive_rows.len() as u64);
            }

            let cache_responses = cache_rows.into_iter().map(|buffered| {
                let height = buffered.height;
                let finality = BlockFinality::for_block(height, head, false);
                let cursor = Cursor::new(buffered.height, buffered.block_id);
                let sb = StreamableBlock {
                    block: buffered,
                    step: Step::New,
                    finality,
                    cursor,
                };
                streamable_to_response(sb, ForkStep::StepNew)
            });
            archive_rows.into_iter().chain(cache_responses).collect()
        } else {
            Vec::new()
        };

        let last_backfill_height = backfill
            .last()
            .and_then(|r| r.metadata.as_ref().map(|m| m.num));

        // Live stream with dedup against the backfill range.
        let live_stream = BroadcastStream::new(live_rx).filter_map(move |res| match res {
            Ok(output) => {
                // If we backfilled up to height B, drop any live
                // emission with height ≤ B (it was already shipped
                // from the cache snapshot).
                let response = output_to_response(output)?;
                match (last_backfill_height, response.metadata.as_ref()) {
                    (Some(b), Some(m)) if m.num <= b => None,
                    _ => Some(Ok(response)),
                }
            }
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                warn!(skipped, "firehose subscriber lagged");
                metrics::counter!(
                    "lightcycle_firehose_lagged_total",
                    "skipped" => skipped.to_string()
                )
                .increment(1);
                Some(Err(Status::resource_exhausted(format!(
                    "subscriber lagged the firehose hub by {skipped} messages; \
                     reconnect with last cursor",
                ))))
            }
        });

        // Chain backfill (already-materialized Vec) onto the live
        // stream. The backfill items are emitted in order; live
        // begins after.
        let backfill_stream = tokio_stream::iter(backfill.into_iter().map(Ok));
        let combined = backfill_stream.chain(live_stream);

        Ok(tonic::Response::new(Box::pin(combined)))
    }
}

/// Walk the archive over `[lo, hi]` (inclusive) and project each row
/// into a `Response`. Each archived value is `pb::Block`-encoded
/// (written by the archiver task on `Output::Irreversible`); we
/// re-emit those bytes directly as `Response.block.value` and decode
/// only enough to populate `Response.metadata`. Cursor + step are
/// reconstructed from the archive's known properties (archived blocks
/// are by construction finalized with `fork_id = 0`).
fn archive_walk(
    archive: &BlockArchive,
    lo: BlockHeight,
    hi: BlockHeight,
) -> Result<Vec<Response>, lightcycle_store::ArchiveError> {
    use lightcycle_proto::sf::tron::type_v1 as pb;
    use lightcycle_types::BlockId;

    // Cap per-call walk size. 5000 archived blocks ≈ 4h at 3-second
    // slot times — large enough for any sane resume, small enough that
    // we don't OOM on a misbehaving consumer asking for everything.
    const ARCHIVE_WALK_LIMIT: usize = 5000;

    let rows = archive.range(lo, hi, ARCHIVE_WALK_LIMIT)?;
    let mut out = Vec::with_capacity(rows.len());
    for (height, block_id, payload) in rows {
        // Decode just enough to populate metadata. pb::Block parses
        // cheaply; we use it as the source of truth for parent_id,
        // time, and the embedded finality.solidified_head_number.
        let pb_block = match pb::Block::decode(payload.as_slice()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(height, error = ?e, "archive payload failed to decode; skipping");
                metrics::counter!("lightcycle_firehose_archive_decode_errors_total").increment(1);
                continue;
            }
        };
        let parent_id_bytes: [u8; 32] = pb_block
            .parent_id
            .as_slice()
            .try_into()
            .unwrap_or([0u8; 32]);
        let lib_num = pb_block
            .finality
            .as_ref()
            .map(|f| f.solidified_head_number)
            .unwrap_or(0);
        let metadata = BlockMetadata {
            num: height,
            id: hex::encode(block_id.0),
            parent_num: height.saturating_sub(1),
            parent_id: hex::encode(parent_id_bytes),
            lib_num,
            time: pb_block.time,
        };
        let block_any = Any {
            type_url: BLOCK_TYPE_URL.into(),
            value: payload,
        };
        let cursor = Cursor::new(height, BlockId(block_id.0));
        out.push(Response {
            block: Some(block_any),
            step: ForkStep::StepNew as i32,
            cursor: hex::encode(cursor.to_bytes()),
            metadata: Some(metadata),
        });
    }
    Ok(out)
}

/// Encode a single StreamableBlock to a Firehose Response. Used by
/// the backfill walk; live-stream emissions go through
/// [`output_to_response`].
fn streamable_to_response(sb: StreamableBlock, step: ForkStep) -> Response {
    let height = sb.block.height;
    let parent_height = height.saturating_sub(1);
    let block_id_hex = hex::encode(sb.block.block_id.0);
    let parent_id_hex = hex::encode(sb.block.parent_id.0);
    let metadata = BlockMetadata {
        num: height,
        id: block_id_hex,
        parent_num: parent_height,
        parent_id: parent_id_hex,
        lib_num: sb.finality.solidified_head.unwrap_or(0),
        time: Some(timestamp_from_ms(sb.block.decoded.header.timestamp_ms)),
    };
    let block_pb = encode_block(&sb.block, sb.finality);
    let block_any = Any {
        type_url: BLOCK_TYPE_URL.into(),
        value: block_pb.encode_to_vec(),
    };
    Response {
        block: Some(block_any),
        step: step as i32,
        cursor: hex::encode(sb.cursor.to_bytes()),
        metadata: Some(metadata),
    }
}

fn output_to_response(output: Output) -> Option<Response> {
    let (step, sb) = match output {
        Output::New(s) => (ForkStep::StepNew, s),
        Output::Undo(s) => (ForkStep::StepUndo, s),
        Output::Irreversible(s) => (ForkStep::StepFinal, s),
        // Ledger-entry variants don't fit Firehose v2's wire shape
        // (no STATUS variant). They flow through tracing logs +
        // metrics in the relayer instead.
        Output::ForkObserved { .. } | Output::ForkResolved { .. } => return None,
    };
    let height = sb.block.height;
    let parent_height = height.saturating_sub(1);
    let block_id_hex = hex::encode(sb.block.block_id.0);
    let parent_id_hex = hex::encode(sb.block.parent_id.0);

    // lib_num: best-effort — we don't carry the last-irreversible
    // height through the StreamableBlock yet. Leaving 0 is honest
    // (consumers will treat as "unknown / pre-finality"); when we
    // thread the LIB through the engine output, fill it in here.
    // lib_num: now sourced from the chain's solidified head when
    // available (per ADR-0021 the only legal finality oracle), so
    // consumers reading metadata.lib_num see the chain's claim, not a
    // count we computed. Falls back to 0 only when the engine hasn't
    // yet seen a successful WalletSolidity.GetNowBlock response.
    let metadata = BlockMetadata {
        num: height,
        id: block_id_hex,
        parent_num: parent_height,
        parent_id: parent_id_hex,
        lib_num: sb.finality.solidified_head.unwrap_or(0),
        time: Some(timestamp_from_ms(sb.block.decoded.header.timestamp_ms)),
    };

    let block_pb = encode_block(&sb.block, sb.finality);
    let block_any = Any {
        type_url: BLOCK_TYPE_URL.into(),
        value: block_pb.encode_to_vec(),
    };

    Some(Response {
        block: Some(block_any),
        step: step as i32,
        cursor: hex::encode(sb.cursor.to_bytes()),
        metadata: Some(metadata),
    })
}

fn timestamp_from_ms(ms: i64) -> Timestamp {
    Timestamp {
        seconds: ms / 1000,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}

/// `Fetch.Block` service: point-in-time block lookup. Holds a shared
/// [`BlockOracle`](crate::oracle::BlockOracle) the call delegates to.
///
/// `Block` returns the canonical version of the block at the requested
/// height, encoded into the same `sf.tron.type.v1.Block` `Any` payload
/// that `Stream.Blocks` ships. Status mapping:
///
/// - `Ok(None)` from the oracle → `Status::not_found` (the chain has no
///   block at that height yet, or the upstream returned nothing)
/// - `Err(_)` from the oracle → `Status::internal` (RPC, decode, etc.)
/// - `block_hash_and_number` / `cursor` references →
///   `Status::failed_precondition` (v0.1 only resolves by number)
/// - `transforms` non-empty → `Status::failed_precondition`
#[derive(Clone)]
pub struct FetchService {
    oracle: SharedBlockOracle,
    /// Persistent block archive. On a cache miss inside the oracle's
    /// caching decorator, the archive is checked before the upstream
    /// RPC is invoked. None falls back to oracle-only behavior (the
    /// v0.x default).
    archive: Option<BlockArchive>,
}

impl std::fmt::Debug for FetchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchService")
            .field("archive", &self.archive.is_some())
            .finish()
    }
}

impl FetchService {
    pub fn new(oracle: SharedBlockOracle) -> Self {
        Self {
            oracle,
            archive: None,
        }
    }

    /// Attach a persistent block archive. Fetch.Block requests for
    /// heights past the in-memory cache are served from the archive
    /// instead of round-tripping to the upstream RPC.
    pub fn with_archive(mut self, archive: BlockArchive) -> Self {
        self.archive = Some(archive);
        self
    }
}

#[tonic::async_trait]
impl FetchSvc for FetchService {
    async fn block(
        &self,
        request: tonic::Request<SingleBlockRequest>,
    ) -> Result<tonic::Response<SingleBlockResponse>, Status> {
        let req = request.into_inner();

        if !req.transforms.is_empty() {
            return Err(Status::failed_precondition(
                "transforms not implemented in v0.1",
            ));
        }

        let height = match req.reference {
            Some(Reference::BlockNumber(bn)) => bn.num,
            Some(Reference::BlockHashAndNumber(_)) => {
                return Err(Status::failed_precondition(
                    "block_hash_and_number not implemented in v0.1; use block_number",
                ));
            }
            Some(Reference::Cursor(_)) => {
                return Err(Status::failed_precondition(
                    "cursor reference not implemented in v0.1; use block_number",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "no reference provided; one of block_number / block_hash_and_number / \
                     cursor required",
                ));
            }
        };

        metrics::counter!("lightcycle_firehose_fetch_total", "result" => "in_flight").increment(1);
        let started = std::time::Instant::now();

        // Archive fast-path: serves any block past the chain's
        // finality threshold without a round-trip to the upstream RPC.
        // Cache (held by the oracle's caching decorator) is still
        // checked next on archive miss, so latest-head fetches retain
        // sub-millisecond response time.
        if let Some(archive) = &self.archive {
            match archive.get(height) {
                Ok(Some((block_id, payload))) => {
                    let elapsed = started.elapsed().as_secs_f64();
                    metrics::histogram!("lightcycle_firehose_fetch_duration_seconds")
                        .record(elapsed);
                    metrics::counter!(
                        "lightcycle_firehose_fetch_total",
                        "result" => "archive_hit"
                    )
                    .increment(1);
                    return Ok(tonic::Response::new(archive_hit_response(
                        height, block_id, payload,
                    )));
                }
                Ok(None) => {
                    // Fall through to oracle.
                }
                Err(e) => {
                    warn!(error = ?e, height, "archive read error; falling through to oracle");
                    metrics::counter!("lightcycle_firehose_fetch_total", "result" => "archive_error")
                        .increment(1);
                }
            }
        }

        let outcome = self.oracle.fetch_block_by_number(height).await;

        let elapsed = started.elapsed().as_secs_f64();
        metrics::histogram!("lightcycle_firehose_fetch_duration_seconds").record(elapsed);

        match outcome {
            Ok(Some((buffered, finality))) => {
                let block_id_hex = hex::encode(buffered.block_id.0);
                let parent_id_hex = hex::encode(buffered.parent_id.0);
                let timestamp_ms = buffered.decoded.header.timestamp_ms;
                let metadata = BlockMetadata {
                    num: buffered.height,
                    id: block_id_hex,
                    parent_num: buffered.height.saturating_sub(1),
                    parent_id: parent_id_hex,
                    lib_num: finality.solidified_head.unwrap_or(0),
                    time: Some(timestamp_from_ms(timestamp_ms)),
                };
                let block_pb = encode_block(&buffered, finality);
                let block_any = Any {
                    type_url: BLOCK_TYPE_URL.into(),
                    value: block_pb.encode_to_vec(),
                };
                metrics::counter!("lightcycle_firehose_fetch_total", "result" => "ok").increment(1);
                Ok(tonic::Response::new(SingleBlockResponse {
                    block: Some(block_any),
                    metadata: Some(metadata),
                }))
            }
            Ok(None) => {
                metrics::counter!("lightcycle_firehose_fetch_total", "result" => "not_found")
                    .increment(1);
                Err(Status::not_found(format!(
                    "block {height} not available upstream"
                )))
            }
            Err(e) => {
                warn!(error = ?e, height, "Fetch.Block oracle error");
                metrics::counter!("lightcycle_firehose_fetch_total", "result" => "error")
                    .increment(1);
                Err(Status::internal(format!("oracle error: {e}")))
            }
        }
    }
}

/// Build a `SingleBlockResponse` from an archived `pb::Block` payload.
/// Same projection logic as `archive_walk` but for the Fetch.Block
/// surface — the metadata is decoded out of the payload, the bytes are
/// passed through verbatim as the `Response.block.value`.
fn archive_hit_response(
    height: BlockHeight,
    block_id: lightcycle_types::BlockId,
    payload: Vec<u8>,
) -> SingleBlockResponse {
    use lightcycle_proto::sf::tron::type_v1 as pb;

    let pb_block = pb::Block::decode(payload.as_slice()).ok();
    let parent_id_bytes: [u8; 32] = pb_block
        .as_ref()
        .and_then(|b| b.parent_id.as_slice().try_into().ok())
        .unwrap_or([0u8; 32]);
    let lib_num = pb_block
        .as_ref()
        .and_then(|b| b.finality.as_ref().map(|f| f.solidified_head_number))
        .unwrap_or(0);
    let time = pb_block.as_ref().and_then(|b| b.time);
    let metadata = BlockMetadata {
        num: height,
        id: hex::encode(block_id.0),
        parent_num: height.saturating_sub(1),
        parent_id: hex::encode(parent_id_bytes),
        lib_num,
        time,
    };
    SingleBlockResponse {
        block: Some(Any {
            type_url: BLOCK_TYPE_URL.into(),
            value: payload,
        }),
        metadata: Some(metadata),
    }
}

/// Minimal endpoint-info service. Reports chain identity so
/// orchestrators can sanity-check the connection.
#[derive(Clone, Debug)]
pub struct EndpointInfoService {
    chain_name: String,
}

impl EndpointInfoService {
    pub fn new(chain_name: impl Into<String>) -> Self {
        Self {
            chain_name: chain_name.into(),
        }
    }
}

#[tonic::async_trait]
impl EndpointInfo for EndpointInfoService {
    async fn info(
        &self,
        _request: tonic::Request<InfoRequest>,
    ) -> Result<tonic::Response<InfoResponse>, Status> {
        Ok(tonic::Response::new(InfoResponse {
            chain_name: self.chain_name.clone(),
            chain_name_aliases: vec![],
            // first_streamable_block_num/id left at 0/empty: v0.1
            // doesn't track upstream chain genesis. Consumers that
            // care should query the upstream node directly.
            first_streamable_block_num: 0,
            first_streamable_block_id: String::new(),
            block_id_encoding: BlockIdEncoding::Hex as i32,
            block_features: vec!["lightcycle-v0".into()],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_codec::{DecodedBlock, DecodedHeader};
    use lightcycle_relayer::{BufferedBlock, Cursor, StreamableBlock};
    use lightcycle_types::{Address, BlockFinality, BlockId, FinalityTier, Step};

    fn synth_output(step: Step, height: u64) -> Output {
        let block = BufferedBlock {
            height,
            block_id: BlockId([height as u8; 32]),
            parent_id: BlockId([(height - 1) as u8; 32]),
            fork_id: 0,
            decoded: DecodedBlock {
                header: DecodedHeader {
                    height,
                    block_id: BlockId([height as u8; 32]),
                    parent_id: BlockId([(height - 1) as u8; 32]),
                    raw_data_hash: [0u8; 32],
                    tx_trie_root: [0u8; 32],
                    timestamp_ms: 1_777_854_558_000,
                    witness: Address([0x41; 21]),
                    witness_signature: vec![],
                    version: 34,
                },
                transactions: vec![],
            },
            tx_infos: vec![],
        };
        let sb = StreamableBlock {
            step,
            cursor: Cursor::new(height, BlockId([height as u8; 32])),
            block,
            finality: BlockFinality {
                tier: FinalityTier::Seen,
                solidified_head: None,
            },
        };
        match step {
            Step::New => Output::New(sb),
            Step::Undo => Output::Undo(sb),
            Step::Irreversible => Output::Irreversible(sb),
        }
    }

    #[test]
    fn output_to_response_maps_step_to_fork_step() {
        let r = output_to_response(synth_output(Step::New, 100)).expect("Some");
        assert_eq!(r.step, ForkStep::StepNew as i32);
        let r = output_to_response(synth_output(Step::Undo, 100)).expect("Some");
        assert_eq!(r.step, ForkStep::StepUndo as i32);
        let r = output_to_response(synth_output(Step::Irreversible, 100)).expect("Some");
        assert_eq!(r.step, ForkStep::StepFinal as i32);
    }

    #[test]
    fn response_metadata_carries_height_and_ids() {
        let r = output_to_response(synth_output(Step::New, 82_531_247)).expect("Some");
        let md = r.metadata.expect("metadata");
        assert_eq!(md.num, 82_531_247);
        assert_eq!(md.parent_num, 82_531_246);
        assert_eq!(md.id.len(), 64); // 32 bytes hex-encoded
        assert!(md.time.is_some());
    }

    #[test]
    fn timestamp_conversion_handles_subsecond() {
        let t = timestamp_from_ms(1_777_854_558_345);
        assert_eq!(t.seconds, 1_777_854_558);
        assert_eq!(t.nanos, 345_000_000);
    }

    #[test]
    fn response_cursor_round_trips_hex() {
        let r = output_to_response(synth_output(Step::New, 100)).expect("Some");
        let bytes = hex::decode(&r.cursor).expect("hex");
        let parsed = Cursor::from_bytes(&bytes).expect("parse");
        assert_eq!(parsed.height, 100);
    }

    #[test]
    fn response_block_carries_sf_tron_payload() {
        // The placeholder Any was replaced by a real
        // sf.tron.type.v1.Block payload. Assert the type_url + that the
        // value bytes round-trip through prost back to a Block whose
        // height matches the synthetic source.
        use lightcycle_proto::sf::tron::type_v1 as tron_v1;
        use prost::Message;

        let r = output_to_response(synth_output(Step::New, 82_500_999)).expect("Some");
        let any = r.block.expect("block any");
        assert_eq!(any.type_url, BLOCK_TYPE_URL);
        assert!(!any.value.is_empty(), "block payload must not be empty");
        let decoded = tron_v1::Block::decode(any.value.as_slice()).expect("decode");
        assert_eq!(decoded.number, 82_500_999);
        assert!(decoded.header.is_some());
    }

    #[test]
    fn response_block_finality_field_round_trips() {
        // ADR-0021 §1: finality tier is on the wire on every
        // emission. Synthesize a block whose StreamableBlock has a
        // populated finality envelope and confirm round-trip.
        use lightcycle_proto::sf::tron::type_v1 as tron_v1;
        use prost::Message;

        let block = BufferedBlock {
            height: 100,
            block_id: BlockId([100u8; 32]),
            parent_id: BlockId([99u8; 32]),
            fork_id: 0,
            decoded: DecodedBlock {
                header: DecodedHeader {
                    height: 100,
                    block_id: BlockId([100u8; 32]),
                    parent_id: BlockId([99u8; 32]),
                    raw_data_hash: [0u8; 32],
                    tx_trie_root: [0u8; 32],
                    timestamp_ms: 1_777_854_558_000,
                    witness: Address([0x41; 21]),
                    witness_signature: vec![],
                    version: 34,
                },
                transactions: vec![],
            },
            tx_infos: vec![],
        };
        let sb = StreamableBlock {
            step: Step::Irreversible,
            cursor: Cursor::new(100, BlockId([100u8; 32])),
            block,
            finality: BlockFinality {
                tier: FinalityTier::Finalized,
                solidified_head: Some(105),
            },
        };
        let resp = output_to_response(Output::Irreversible(sb)).expect("Some");
        // metadata.lib_num is now sourced from solidified_head.
        assert_eq!(resp.metadata.as_ref().unwrap().lib_num, 105);
        let any = resp.block.expect("block");
        let decoded = tron_v1::Block::decode(any.value.as_slice()).expect("decode");
        let f = decoded.finality.expect("finality on wire");
        assert_eq!(f.tier, tron_v1::FinalityTier::Finalized as i32);
        assert_eq!(f.solidified_head_number, 105);
    }

    #[test]
    fn ledger_entries_skip_grpc_response() {
        // ForkObserved / ForkResolved have no Firehose v2 wire shape;
        // output_to_response returns None so the gRPC stream filters
        // them out.
        let observed = Output::ForkObserved {
            observed_at_height: 100,
            kept_tip: BlockId([0xbb; 32]),
            orphaned_tips: vec![BlockId([0xaa; 32])],
        };
        assert!(output_to_response(observed).is_none());

        let resolved = Output::ForkResolved {
            observed_at_height: 100,
            finalized_head: 120,
            finalized_tip: BlockId([0xbb; 32]),
            orphaned_tips: vec![BlockId([0xaa; 32])],
        };
        assert!(output_to_response(resolved).is_none());
    }

    // -- Fetch.Block service ----------------------------------------

    use crate::oracle::BlockOracle;
    use lightcycle_proto::firehose::v2::single_block_request::{
        BlockHashAndNumber, BlockNumber, Cursor as CursorRef,
    };
    use std::sync::Arc;
    use tonic::Code;

    fn synth_buffered(height: u64) -> BufferedBlock {
        BufferedBlock {
            height,
            block_id: BlockId([height as u8; 32]),
            parent_id: BlockId([(height - 1) as u8; 32]),
            fork_id: 0,
            decoded: DecodedBlock {
                header: DecodedHeader {
                    height,
                    block_id: BlockId([height as u8; 32]),
                    parent_id: BlockId([(height - 1) as u8; 32]),
                    raw_data_hash: [0u8; 32],
                    tx_trie_root: [0u8; 32],
                    timestamp_ms: 1_777_854_558_000,
                    witness: Address([0x41; 21]),
                    witness_signature: vec![],
                    version: 34,
                },
                transactions: vec![],
            },
            tx_infos: vec![],
        }
    }

    /// In-memory oracle. Holds a per-height answer; missing keys are
    /// reported as `Ok(None)`. Pairs with [`ErrOracle`] for failure
    /// cases.
    struct VecOracle {
        items: std::collections::HashMap<u64, (BufferedBlock, BlockFinality)>,
    }
    impl VecOracle {
        fn new() -> Self {
            Self {
                items: Default::default(),
            }
        }
        fn with(mut self, h: u64, sb: BufferedBlock, finality: BlockFinality) -> Self {
            self.items.insert(h, (sb, finality));
            self
        }
    }
    #[async_trait::async_trait]
    impl BlockOracle for VecOracle {
        async fn fetch_block_by_number(
            &self,
            height: u64,
        ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>> {
            Ok(self.items.get(&height).cloned())
        }
    }

    struct ErrOracle;
    #[async_trait::async_trait]
    impl BlockOracle for ErrOracle {
        async fn fetch_block_by_number(
            &self,
            _height: u64,
        ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>> {
            Err(anyhow::anyhow!("synthetic upstream failure"))
        }
    }

    fn req_by_number(num: u64) -> tonic::Request<SingleBlockRequest> {
        tonic::Request::new(SingleBlockRequest {
            reference: Some(Reference::BlockNumber(BlockNumber { num })),
            transforms: vec![],
        })
    }

    #[tokio::test]
    async fn fetch_returns_block_with_finality_when_oracle_has_it() {
        let oracle = VecOracle::new().with(
            100,
            synth_buffered(100),
            BlockFinality {
                tier: FinalityTier::Finalized,
                solidified_head: Some(120),
            },
        );
        let svc = FetchService::new(Arc::new(oracle));
        let resp = svc
            .block(req_by_number(100))
            .await
            .expect("ok")
            .into_inner();

        let md = resp.metadata.expect("metadata populated");
        assert_eq!(md.num, 100);
        assert_eq!(md.parent_num, 99);
        assert_eq!(md.id.len(), 64);
        // lib_num sourced from finality.solidified_head per ADR-0021.
        assert_eq!(md.lib_num, 120);

        let any = resp.block.expect("block any populated");
        assert_eq!(any.type_url, BLOCK_TYPE_URL);

        // Round-trip the embedded sf.tron.type.v1.Block to confirm the
        // finality envelope is on the wire end-to-end.
        use lightcycle_proto::sf::tron::type_v1 as tron_v1;
        let decoded = tron_v1::Block::decode(any.value.as_slice()).expect("decode");
        let f = decoded.finality.expect("finality");
        assert_eq!(f.tier, tron_v1::FinalityTier::Finalized as i32);
        assert_eq!(f.solidified_head_number, 120);
    }

    #[tokio::test]
    async fn fetch_returns_not_found_when_oracle_returns_none() {
        let oracle = VecOracle::new(); // no entries
        let svc = FetchService::new(Arc::new(oracle));
        let err = svc.block(req_by_number(100)).await.unwrap_err();
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn fetch_returns_internal_when_oracle_errors() {
        let svc = FetchService::new(Arc::new(ErrOracle));
        let err = svc.block(req_by_number(100)).await.unwrap_err();
        assert_eq!(err.code(), Code::Internal);
    }

    #[tokio::test]
    async fn fetch_rejects_block_hash_and_number_reference() {
        let svc = FetchService::new(Arc::new(VecOracle::new()));
        let req = tonic::Request::new(SingleBlockRequest {
            reference: Some(Reference::BlockHashAndNumber(BlockHashAndNumber {
                num: 100,
                hash: "deadbeef".into(),
            })),
            transforms: vec![],
        });
        let err = svc.block(req).await.unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn fetch_rejects_cursor_reference() {
        let svc = FetchService::new(Arc::new(VecOracle::new()));
        let req = tonic::Request::new(SingleBlockRequest {
            reference: Some(Reference::Cursor(CursorRef {
                cursor: "abc".into(),
            })),
            transforms: vec![],
        });
        let err = svc.block(req).await.unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn fetch_rejects_missing_reference() {
        let svc = FetchService::new(Arc::new(VecOracle::new()));
        let req = tonic::Request::new(SingleBlockRequest {
            reference: None,
            transforms: vec![],
        });
        let err = svc.block(req).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn fetch_rejects_non_empty_transforms() {
        let svc = FetchService::new(Arc::new(VecOracle::new()));
        let req = tonic::Request::new(SingleBlockRequest {
            reference: Some(Reference::BlockNumber(BlockNumber { num: 100 })),
            transforms: vec![Any {
                type_url: "x".into(),
                value: vec![],
            }],
        });
        let err = svc.block(req).await.unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition);
    }
}
