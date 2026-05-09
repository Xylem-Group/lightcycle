//! Firehose v2 gRPC server: live `Stream.Blocks` + `EndpointInfo.Info`.
//!
//! v0.1 surface:
//!
//! - **`Stream.Blocks`** — live mode only. The request's `cursor` and
//!   `start_block_num` fields are not honored yet; backfill / replay
//!   from a saved cursor lands when we wire the [`lightcycle-store`]
//!   crate into the pipeline. Requests that supply either field get
//!   a `FailedPrecondition` status with a clear message rather than
//!   silent best-effort behavior.
//! - **`EndpointInfo.Info`** — minimal: chain name, BlockIdEncoding=HEX,
//!   first_streamable left at 0/empty (we don't track upstream chain
//!   genesis at v0.1).
//! - **`Fetch.Block`** — not implemented. The proto compiles in
//!   without us serving it; the route just isn't bound. Lands when
//!   block storage is in place.
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
    endpoint_info_server::EndpointInfo, info_response::BlockIdEncoding,
    stream_server::Stream as StreamSvc, BlockMetadata, ForkStep, InfoRequest, InfoResponse,
    Request, Response,
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
        let stream = BroadcastStream::new(rx).map(|res| match res {
            Ok(output) => Ok(output_to_response(output)),
            Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                warn!(skipped, "firehose subscriber lagged");
                metrics::counter!(
                    "lightcycle_firehose_lagged_total",
                    "skipped" => skipped.to_string()
                )
                .increment(1);
                Err(Status::resource_exhausted(format!(
                    "subscriber lagged the firehose hub by {skipped} messages; \
                     reconnect with last cursor",
                )))
            }
        });

        Ok(tonic::Response::new(Box::pin(stream)))
    }
}

fn output_to_response(output: Output) -> Response {
    let (step, sb) = match output {
        Output::New(s) => (ForkStep::StepNew, s),
        Output::Undo(s) => (ForkStep::StepUndo, s),
        Output::Irreversible(s) => (ForkStep::StepFinal, s),
    };
    let height = sb.block.height;
    let parent_height = height.saturating_sub(1);
    let block_id_hex = hex::encode(sb.block.block_id.0);
    let parent_id_hex = hex::encode(sb.block.parent_id.0);

    // lib_num: best-effort — we don't carry the last-irreversible
    // height through the StreamableBlock yet. Leaving 0 is honest
    // (consumers will treat as "unknown / pre-finality"); when we
    // thread the LIB through the engine output, fill it in here.
    let metadata = BlockMetadata {
        num: height,
        id: block_id_hex,
        parent_num: parent_height,
        parent_id: parent_id_hex,
        lib_num: 0,
        time: Some(timestamp_from_ms(sb.block.decoded.header.timestamp_ms)),
    };

    let block_pb = encode_block(&sb.block);
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

fn timestamp_from_ms(ms: i64) -> Timestamp {
    Timestamp {
        seconds: ms / 1000,
        nanos: ((ms % 1000) * 1_000_000) as i32,
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
    use lightcycle_types::{Address, BlockId, Step};

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
        };
        let sb = StreamableBlock {
            step,
            cursor: Cursor::new(height, BlockId([height as u8; 32])),
            block,
        };
        match step {
            Step::New => Output::New(sb),
            Step::Undo => Output::Undo(sb),
            Step::Irreversible => Output::Irreversible(sb),
        }
    }

    #[test]
    fn output_to_response_maps_step_to_fork_step() {
        let r = output_to_response(synth_output(Step::New, 100));
        assert_eq!(r.step, ForkStep::StepNew as i32);
        let r = output_to_response(synth_output(Step::Undo, 100));
        assert_eq!(r.step, ForkStep::StepUndo as i32);
        let r = output_to_response(synth_output(Step::Irreversible, 100));
        assert_eq!(r.step, ForkStep::StepFinal as i32);
    }

    #[test]
    fn response_metadata_carries_height_and_ids() {
        let r = output_to_response(synth_output(Step::New, 82_531_247));
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
        let r = output_to_response(synth_output(Step::New, 100));
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

        let r = output_to_response(synth_output(Step::New, 82_500_999));
        let any = r.block.expect("block any");
        assert_eq!(any.type_url, BLOCK_TYPE_URL);
        assert!(!any.value.is_empty(), "block payload must not be empty");
        let decoded = tron_v1::Block::decode(any.value.as_slice()).expect("decode");
        assert_eq!(decoded.number, 82_500_999);
        assert!(decoded.header.is_some());
    }
}
