//! End-to-end gRPC round-trip: spin up the firehose server on a
//! local ephemeral port, connect a tonic client, push a synthetic
//! Output through the hub, assert the client receives a Response
//! with the right step + metadata.
//!
//! Skipping `start_block_num` / cursor / final_blocks_only paths —
//! those are explicitly rejected at v0.1 and have unit-test coverage
//! in `src/server.rs`. This test covers the live happy path.

use std::time::Duration;

use lightcycle_codec::{DecodedBlock, DecodedHeader};
use lightcycle_firehose::{serve, Hub, BLOCK_TYPE_URL};
use lightcycle_proto::firehose::v2::{
    endpoint_info_client::EndpointInfoClient, stream_client::StreamClient, ForkStep, InfoRequest,
    Request,
};
use lightcycle_proto::sf::tron::type_v1 as tron_v1;
use lightcycle_relayer::{BufferedBlock, Cursor, Output, StreamableBlock};
use lightcycle_types::{Address, BlockFinality, BlockId, FinalityTier, Step};
use prost::Message;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;

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

async fn pick_addr() -> std::net::SocketAddr {
    // Bind to port 0 to let the OS pick, immediately read back.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

#[tokio::test]
async fn live_stream_round_trip() {
    let hub = Hub::new(64);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(addr, hub, "tron-test", async move {
                let _ = shutdown_rx.await;
            })
            .await;
        }
    });

    // Wait briefly for the server to bind. tonic doesn't expose a
    // "ready" signal; a short retry loop is the standard idiom.
    let mut client = None;
    for _ in 0..40 {
        if let Ok(c) = StreamClient::connect(format!("http://{addr}")).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut client = client.expect("server didn't accept connections");

    let mut stream = client
        .blocks(Request {
            start_block_num: 0,
            cursor: String::new(),
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect("blocks rpc")
        .into_inner();

    // Push an Output via the hub's underlying broadcast directly. We
    // can't use pump_from here because there's no upstream mpsc — the
    // server-side test is just the gRPC layer.
    hub.sender()
        .send(synth_output(Step::New, 82_531_247))
        .expect("broadcast send: client subscribed, recv exists");

    let resp = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream.next() timeout")
        .expect("stream ended unexpectedly")
        .expect("stream item is Err");

    assert_eq!(resp.step, ForkStep::StepNew as i32);
    let md = resp.metadata.expect("metadata");
    assert_eq!(md.num, 82_531_247);
    assert_eq!(md.parent_num, 82_531_246);
    assert_eq!(md.id.len(), 64);
    assert!(!resp.cursor.is_empty(), "cursor should be hex-encoded");

    // Response.block must carry the chain-specific TRON proto, not
    // the old placeholder. type_url + non-empty value + decodable as
    // sf.tron.type.v1.Block with matching height.
    let block_any = resp.block.expect("block payload");
    assert_eq!(block_any.type_url, BLOCK_TYPE_URL);
    let decoded =
        tron_v1::Block::decode(block_any.value.as_slice()).expect("decode sf.tron.type.v1.Block");
    assert_eq!(decoded.number, 82_531_247);
    assert_eq!(decoded.id.len(), 32);
    assert_eq!(decoded.parent_id.len(), 32);
    assert!(decoded.header.is_some());
    assert!(decoded.transactions.is_empty(), "synth block had no txs");

    // Shut the server down cleanly.
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn endpoint_info_round_trip() {
    let hub = Hub::new(8);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(addr, hub, "tron-mainnet", async move {
                let _ = shutdown_rx.await;
            })
            .await;
        }
    });

    let mut client = None;
    for _ in 0..40 {
        if let Ok(c) = EndpointInfoClient::connect(format!("http://{addr}")).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut client = client.expect("server didn't accept connections");

    let resp = client
        .info(InfoRequest {})
        .await
        .expect("info rpc")
        .into_inner();
    assert_eq!(resp.chain_name, "tron-mainnet");
    assert_eq!(resp.block_features, vec!["lightcycle-v0".to_string()]);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn live_stream_rejects_cursor_request() {
    let hub = Hub::new(8);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(addr, hub, "tron-test", async move {
                let _ = shutdown_rx.await;
            })
            .await;
        }
    });

    let mut client = None;
    for _ in 0..40 {
        if let Ok(c) = StreamClient::connect(format!("http://{addr}")).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut client = client.expect("server didn't accept connections");

    let err = client
        .blocks(Request {
            start_block_num: 0,
            cursor: "deadbeef".into(), // non-empty cursor
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .err()
        .expect("expected FailedPrecondition for cursor=...");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}
