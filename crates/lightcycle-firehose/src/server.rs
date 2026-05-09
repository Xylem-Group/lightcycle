//! Firehose v2 gRPC server: `Stream.Blocks` + `Fetch.Block` + `EndpointInfo.Info`.
//!
//! v0.1 surface:
//!
//! - **`Stream.Blocks`** — live mode only. The request's `cursor` and
//!   `start_block_num` fields are not honored yet; backfill / replay
//!   from a saved cursor lands when we wire the [`lightcycle-store`]
//!   crate into the pipeline. Requests that supply either field get
//!   a `FailedPrecondition` status with a clear message rather than
//!   silent best-effort behavior.
//! - **`Fetch.Block`** — point-in-time block lookup by height. Backed
//!   by a [`BlockOracle`] (the CLI builds one over a dedicated
//!   `GrpcSource` connection; future `lightcycle-store` cache impls
//!   will compose). Only `BlockNumber` references are supported in
//!   v0.1; `BlockHashAndNumber` and `Cursor` references return
//!   `FailedPrecondition`. No transforms.
//! - **`EndpointInfo.Info`** — minimal: chain name, BlockIdEncoding=HEX,
//!   first_streamable left at 0/empty (we don't track upstream chain
//!   genesis at v0.1).
//!
//! ## Response shape
//!
//! `Response.metadata` is fully populated: num, id (hex), parent_num,
//! parent_id (hex), lib_num, time. This is what dashboards and
//! orchestrators read. `Response.block` carries an `Any` whose
//! `type_url` is `sf.tron.type.v1.Block` and whose `value` is the
//! prost-encoded block — header, transactions, contracts (typed
//! payloads for the four high-volume contract kinds + raw bytes for
//! everything else). The `Transaction.info` field (logs, internal
//! txs, resource accounting) is unset in v0.1 because the ingest
//! pipeline doesn't yet fetch java-tron's
//! `getTransactionInfoByBlockNum` side channel; that's a follow-up
//! that drops in without a wire change.

use std::pin::Pin;

use lightcycle_proto::firehose::v2::{
    endpoint_info_server::EndpointInfo, fetch_server::Fetch as FetchSvc,
    info_response::BlockIdEncoding, single_block_request::Reference,
    stream_server::Stream as StreamSvc, BlockMetadata, ForkStep, InfoRequest, InfoResponse,
    Request, Response, SingleBlockRequest, SingleBlockResponse,
};
use lightcycle_relayer::Output;
use prost::Message;
use prost_types::{Any, Timestamp};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::{Stream, StreamExt};
use tonic::Status;
use tracing::{debug, warn};

use crate::encode::{encode_block, BLOCK_TYPE_URL};
use crate::hub::Hub;
use crate::oracle::SharedBlockOracle;

/// Stream service. Holds a clone of the hub so each `blocks` RPC
/// call subscribes to the live broadcast.
#[derive(Clone, Debug)]
pub struct StreamService {
    hub: Hub,
}

impl StreamService {
    pub fn new(hub: Hub) -> Self {
        Self { hub }
    }
}

#[tonic::async_trait]
impl StreamSvc for StreamService {
    type BlocksStream =
        Pin<Box<dyn Stream<Item = Result<Response, Status>> + Send + 'static>>;

    async fn blocks(
        &self,
        request: tonic::Request<Request>,
    ) -> Result<tonic::Response<Self::BlocksStream>, Status> {
        let req = request.into_inner();

        // v0.1: live mode only. Reject any request that asks for
        // backfill so we fail fast rather than silently dropping
        // the cursor/start_block_num intent on the floor.
        if !req.cursor.is_empty() {
            return Err(Status::failed_precondition(
                "backfill via cursor not implemented in v0.1; live-from-now only",
            ));
        }
        if req.start_block_num != 0 {
            return Err(Status::failed_precondition(
                "start_block_num not implemented in v0.1; live-from-now only",
            ));
        }
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

        debug!("new firehose subscriber connecting");
        metrics::counter!("lightcycle_firehose_subscriptions_total").increment(1);

        let rx = self.hub.subscribe();
        // tokio_stream's BroadcastStream wraps the Receiver in a
        // proper Stream impl that holds the recv future across polls
        // — necessary so the broadcast send wakes us on the next
        // message rather than silently dropping the waker.
        // Filter ledger-entry variants (ForkObserved / ForkResolved) at
        // the gRPC boundary: Firehose v2's `Response` has no STATUS
        // shape, and the audit ledger flows through tracing logs +
        // metrics on the relayer side instead. See ARCHITECTURE.md
        // "Finality discipline" + ADR-0021 §3.
        let stream = BroadcastStream::new(rx).filter_map(|res| match res {
            Ok(output) => output_to_response(output).map(Ok),
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

        Ok(tonic::Response::new(Box::pin(stream)))
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
}

impl std::fmt::Debug for FetchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchService").finish()
    }
}

impl FetchService {
    pub fn new(oracle: SharedBlockOracle) -> Self {
        Self { oracle }
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

        metrics::counter!("lightcycle_firehose_fetch_total", "result" => "in_flight")
            .increment(1);
        let started = std::time::Instant::now();

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
                metrics::counter!("lightcycle_firehose_fetch_total", "result" => "ok")
                    .increment(1);
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
        let resp = svc.block(req_by_number(100)).await.expect("ok").into_inner();

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
