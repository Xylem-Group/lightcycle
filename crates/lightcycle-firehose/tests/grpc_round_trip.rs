//! End-to-end gRPC round-trip: spin up the firehose server on a
//! local ephemeral port, connect a tonic client, push a synthetic
//! Output through the hub, assert the client receives a Response
//! with the right step + metadata.
//!
//! Covers: live happy path; Fetch.Block round-trip; EndpointInfo;
//! malformed-cursor rejection; backfill-without-cache rejection;
//! in-cache backfill walk; live/cache overlap dedup.
//! `final_blocks_only`, `transforms`, and stop_block_num are unit-
//! tested in `src/server.rs` (server-side reject paths).

use std::sync::Arc;
use std::time::Duration;

use lightcycle_codec::{DecodedBlock, DecodedHeader};
use lightcycle_firehose::{serve, BlockOracle, Hub, SharedBlockOracle, BLOCK_TYPE_URL};
use lightcycle_proto::firehose::v2::{
    endpoint_info_client::EndpointInfoClient,
    fetch_client::FetchClient,
    single_block_request::{BlockNumber, Reference},
    stream_client::StreamClient,
    ForkStep, InfoRequest, Request, SingleBlockRequest,
};
use lightcycle_proto::sf::tron::type_v1 as tron_v1;
use lightcycle_relayer::{BufferedBlock, Cursor, Output, StreamableBlock};
use lightcycle_types::{Address, BlockFinality, BlockId, FinalityTier, Step};
use prost::Message;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;

/// Tiny in-memory oracle for tests. Holds nothing; every call returns
/// `Ok(None)`. Used wherever a test only exercises the Stream side and
/// doesn't care about Fetch behavior — `serve()` requires a `SharedBlockOracle`.
struct EmptyOracle;
#[async_trait::async_trait]
impl BlockOracle for EmptyOracle {
    async fn fetch_block_by_number(
        &self,
        _height: u64,
    ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>> {
        Ok(None)
    }
}

fn empty_oracle() -> SharedBlockOracle {
    Arc::new(EmptyOracle) as SharedBlockOracle
}

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
            let _ = serve(addr, hub, empty_oracle(), "tron-test", None, async move {
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
            let _ = serve(
                addr,
                hub,
                empty_oracle(),
                "tron-mainnet",
                None,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
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
async fn fetch_block_round_trip() {
    // Oracle holds one block at height 100 with a finalized envelope.
    // Fetch.Block(num=100) over real gRPC returns it; Fetch.Block(num=999)
    // returns NotFound; Fetch.Block(cursor=...) returns FailedPrecondition.
    struct OneBlockOracle;
    #[async_trait::async_trait]
    impl BlockOracle for OneBlockOracle {
        async fn fetch_block_by_number(
            &self,
            height: u64,
        ) -> anyhow::Result<Option<(BufferedBlock, BlockFinality)>> {
            if height == 100 {
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
                Ok(Some((
                    block,
                    BlockFinality {
                        tier: FinalityTier::Finalized,
                        solidified_head: Some(120),
                    },
                )))
            } else {
                Ok(None)
            }
        }
    }

    let hub = Hub::new(8);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(
                addr,
                hub,
                Arc::new(OneBlockOracle) as SharedBlockOracle,
                "tron-test",
                None,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await;
        }
    });

    let mut client = None;
    for _ in 0..40 {
        if let Ok(c) = FetchClient::connect(format!("http://{addr}")).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut client = client.expect("server didn't accept connections");

    // Happy path
    let resp = client
        .block(SingleBlockRequest {
            reference: Some(Reference::BlockNumber(BlockNumber { num: 100 })),
            transforms: vec![],
        })
        .await
        .expect("fetch ok")
        .into_inner();
    let md = resp.metadata.expect("metadata");
    assert_eq!(md.num, 100);
    assert_eq!(md.parent_num, 99);
    assert_eq!(md.lib_num, 120, "lib_num sourced from solidified_head");
    let any = resp.block.expect("block");
    assert_eq!(any.type_url, BLOCK_TYPE_URL);
    let decoded = tron_v1::Block::decode(any.value.as_slice()).expect("decode");
    let f = decoded.finality.expect("finality envelope on wire");
    assert_eq!(f.tier, tron_v1::FinalityTier::Finalized as i32);

    // NotFound for a height the oracle doesn't know about
    let err = client
        .block(SingleBlockRequest {
            reference: Some(Reference::BlockNumber(BlockNumber { num: 999 })),
            transforms: vec![],
        })
        .await
        .expect_err("expected NotFound");
    assert_eq!(err.code(), tonic::Code::NotFound);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn live_stream_rejects_malformed_cursor() {
    // A non-empty cursor that isn't 40 bytes (or isn't valid hex)
    // is a client-side bug; surface as InvalidArgument.
    let hub = Hub::new(8);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(addr, hub, empty_oracle(), "tron-test", None, async move {
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
            cursor: "deadbeef".into(), // 4 bytes, not 40
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect_err("expected InvalidArgument for malformed cursor");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn backfill_rejected_when_no_cache_attached() {
    // start_block_num != 0 but no StreamBackfill wired → reject
    // with FailedPrecondition. Honest "this server isn't configured
    // for backfill" rather than silently doing live-only.
    let hub = Hub::new(8);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(addr, hub, empty_oracle(), "tron-test", None, async move {
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

    // FailedPrecondition surfaces at the rpc-call level (it's
    // produced before we hand back the stream). tonic returns the
    // status from `blocks().await`, not from the stream's first
    // frame.
    let err = client
        .blocks(Request {
            start_block_num: 100,
            cursor: String::new(),
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect_err("expected FailedPrecondition for backfill without cache");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("no block cache"),
        "expected 'no block cache' in error message, got: {}",
        err.message()
    );

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn backfill_walks_cache_and_then_streams_live() {
    // Pre-populate the cache with heights 100..103. Subscribe with
    // start_block_num=100. Expect to receive 100, 101, 102 from the
    // backfill walk; then push 103 onto the hub and expect to
    // receive 103 on the live tail (no duplication).
    let hub = Hub::new(64);
    let cache = lightcycle_store::new_shared::<BufferedBlock>(16);
    {
        let mut g = cache.write().await;
        for h in 100..103u64 {
            let buffered = BufferedBlock {
                height: h,
                block_id: BlockId([h as u8; 32]),
                parent_id: BlockId([(h - 1) as u8; 32]),
                fork_id: 0,
                decoded: DecodedBlock {
                    header: DecodedHeader {
                        height: h,
                        block_id: BlockId([h as u8; 32]),
                        parent_id: BlockId([(h - 1) as u8; 32]),
                        raw_data_hash: [0u8; 32],
                        tx_trie_root: [0u8; 32],
                        timestamp_ms: 1_777_854_558_000 + h as i64,
                        witness: Address([0x41; 21]),
                        witness_signature: vec![],
                        version: 34,
                    },
                    transactions: vec![],
                },
                tx_infos: vec![],
            };
            g.insert(h, buffered.block_id, buffered);
        }
    }
    let (_head_tx, head_rx) =
        tokio::sync::watch::channel::<Option<lightcycle_types::BlockHeight>>(None);
    let backfill = lightcycle_firehose::StreamBackfill {
        cache: cache.clone(),
        solidified_head: head_rx,
        archive: None,
    };

    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(
                addr,
                hub,
                empty_oracle(),
                "tron-test",
                Some(backfill),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
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

    let mut stream = client
        .blocks(Request {
            start_block_num: 100,
            cursor: String::new(),
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect("stream request accepted")
        .into_inner();

    // First three frames come from the backfill walk: 100, 101, 102.
    for expected in 100..103u64 {
        let resp = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("backfill frame timed out")
            .expect("backfill stream ended early")
            .expect("backfill frame is Err");
        assert_eq!(resp.metadata.unwrap().num, expected);
    }

    // Now push live height 103 through the hub. Expect to receive
    // it; expect NOT to receive a duplicate of any backfilled
    // height.
    hub.sender()
        .send(synth_output(Step::New, 103))
        .expect("broadcast send");
    let resp = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("live frame timed out")
        .expect("stream ended")
        .expect("live frame is Err");
    assert_eq!(resp.metadata.unwrap().num, 103);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn backfill_dedups_when_cache_overlaps_live() {
    // Cache has heights 100..103. Push live emissions for 102 and
    // 103 onto the hub BEFORE subscribing (they'll be in the
    // broadcast buffer when our subscriber attaches). Subscribe
    // with start_block_num=100. Expect 100, 101, 102 from
    // backfill; then 103 from live (102 was buffered live but
    // dedup'd).
    let hub = Hub::new(64);
    let cache = lightcycle_store::new_shared::<BufferedBlock>(16);
    {
        let mut g = cache.write().await;
        for h in 100..103u64 {
            let buffered = BufferedBlock {
                height: h,
                block_id: BlockId([h as u8; 32]),
                parent_id: BlockId([(h - 1) as u8; 32]),
                fork_id: 0,
                decoded: DecodedBlock {
                    header: DecodedHeader {
                        height: h,
                        block_id: BlockId([h as u8; 32]),
                        parent_id: BlockId([(h - 1) as u8; 32]),
                        raw_data_hash: [0u8; 32],
                        tx_trie_root: [0u8; 32],
                        timestamp_ms: 1_777_854_558_000 + h as i64,
                        witness: Address([0x41; 21]),
                        witness_signature: vec![],
                        version: 34,
                    },
                    transactions: vec![],
                },
                tx_infos: vec![],
            };
            g.insert(h, buffered.block_id, buffered);
        }
    }
    let (_head_tx, head_rx) =
        tokio::sync::watch::channel::<Option<lightcycle_types::BlockHeight>>(None);
    let backfill = lightcycle_firehose::StreamBackfill {
        cache: cache.clone(),
        solidified_head: head_rx,
        archive: None,
    };

    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let hub_for_serve = hub.clone();
    let server = tokio::spawn(async move {
        let _ = serve(
            addr,
            hub_for_serve,
            empty_oracle(),
            "tron-test",
            Some(backfill),
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await;
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

    let mut stream = client
        .blocks(Request {
            start_block_num: 100,
            cursor: String::new(),
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect("stream request accepted")
        .into_inner();

    // Drain backfill (3 frames: 100, 101, 102).
    let mut backfilled = Vec::new();
    for _ in 0..3 {
        let r = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        backfilled.push(r.metadata.unwrap().num);
    }
    assert_eq!(backfilled, vec![100, 101, 102]);

    // After the subscription is in place, push 102 (overlap) +
    // 103 (new) live. The 102 must be deduplicated; the 103 must
    // arrive.
    hub.sender()
        .send(synth_output(Step::New, 102))
        .expect("broadcast send 102");
    hub.sender()
        .send(synth_output(Step::New, 103))
        .expect("broadcast send 103");

    let resp = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        resp.metadata.unwrap().num,
        103,
        "expected 103 (102 should be dedup'd)"
    );

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn backfill_walks_archive_then_cache_then_live() {
    // Archive holds 50..60 (older finalized blocks).
    // Cache holds 60..63 (recent in-memory window).
    // Subscribe with start_block_num=50.
    // Expect: 50..60 from archive, 60..63 from cache, then 63 from
    // live. (Archive and cache abut at 60; cache wins for 60 since
    // archive_walk emits up to cache.min_height-1 = 59.)
    let dir = tempfile::tempdir().unwrap();
    let archive = lightcycle_store::BlockArchive::open(dir.path().join("archive.redb")).unwrap();
    for h in 50..60u64 {
        let pb = tron_v1::Block {
            number: h,
            id: vec![h as u8; 32],
            parent_id: vec![(h - 1) as u8; 32],
            time: Some(prost_types::Timestamp {
                seconds: 1_777_854_558 + h as i64,
                nanos: 0,
            }),
            header: None,
            transactions: vec![],
            finality: Some(tron_v1::Finality {
                tier: tron_v1::FinalityTier::Finalized as i32,
                solidified_head_number: 100,
            }),
        };
        archive
            .put(h, BlockId([h as u8; 32]), &pb.encode_to_vec())
            .unwrap();
    }

    let cache = lightcycle_store::new_shared::<BufferedBlock>(16);
    {
        let mut g = cache.write().await;
        for h in 60..63u64 {
            let buffered = BufferedBlock {
                height: h,
                block_id: BlockId([h as u8; 32]),
                parent_id: BlockId([(h - 1) as u8; 32]),
                fork_id: 0,
                decoded: DecodedBlock {
                    header: DecodedHeader {
                        height: h,
                        block_id: BlockId([h as u8; 32]),
                        parent_id: BlockId([(h - 1) as u8; 32]),
                        raw_data_hash: [0u8; 32],
                        tx_trie_root: [0u8; 32],
                        timestamp_ms: 1_777_854_558_000 + h as i64,
                        witness: Address([0x41; 21]),
                        witness_signature: vec![],
                        version: 34,
                    },
                    transactions: vec![],
                },
                tx_infos: vec![],
            };
            g.insert(h, buffered.block_id, buffered);
        }
    }

    let hub = Hub::new(64);
    let (_head_tx, head_rx) =
        tokio::sync::watch::channel::<Option<lightcycle_types::BlockHeight>>(None);
    let backfill = lightcycle_firehose::StreamBackfill {
        cache: cache.clone(),
        solidified_head: head_rx,
        archive: Some(archive.clone()),
    };

    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(
                addr,
                hub,
                empty_oracle(),
                "tron-test",
                Some(backfill),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
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

    let mut stream = client
        .blocks(Request {
            start_block_num: 50,
            cursor: String::new(),
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect("stream request accepted")
        .into_inner();

    // 13 backfill frames: 50..60 from archive, 60..63 from cache.
    let mut got = Vec::new();
    for _ in 50..63u64 {
        let r = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("backfill frame timed out")
            .expect("stream ended early")
            .expect("backfill frame is Err");
        got.push(r.metadata.unwrap().num);
    }
    assert_eq!(got, (50..63u64).collect::<Vec<_>>());

    // Live tail: push 63.
    hub.sender()
        .send(synth_output(Step::New, 63))
        .expect("broadcast send");
    let r = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("live frame timed out")
        .expect("stream ended")
        .expect("live frame is Err");
    assert_eq!(r.metadata.unwrap().num, 63);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn fetch_block_archive_hit_short_circuits_oracle() {
    // Pre-populate archive with height 42. FetchService is wired with
    // archive + an oracle that would return None. Expect the Fetch
    // call to succeed with the archived bytes (not a NotFound).
    let dir = tempfile::tempdir().unwrap();
    let archive = lightcycle_store::BlockArchive::open(dir.path().join("a.redb")).unwrap();
    let pb = tron_v1::Block {
        number: 42,
        id: vec![42u8; 32],
        parent_id: vec![41u8; 32],
        time: Some(prost_types::Timestamp {
            seconds: 1_777_854_600,
            nanos: 0,
        }),
        header: None,
        transactions: vec![],
        finality: Some(tron_v1::Finality {
            tier: tron_v1::FinalityTier::Finalized as i32,
            solidified_head_number: 100,
        }),
    };
    archive
        .put(42, BlockId([42u8; 32]), &pb.encode_to_vec())
        .unwrap();

    // Stand the firehose up directly via the lib's `serve` so the
    // archive flows through to FetchService.
    let cache = lightcycle_store::new_shared::<BufferedBlock>(4);
    let (_head_tx, head_rx) =
        tokio::sync::watch::channel::<Option<lightcycle_types::BlockHeight>>(None);
    let backfill = lightcycle_firehose::StreamBackfill {
        cache,
        solidified_head: head_rx,
        archive: Some(archive),
    };
    let hub = Hub::new(16);
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let _ = serve(
            addr,
            hub,
            empty_oracle(),
            "tron-test",
            Some(backfill),
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await;
    });

    let mut client = None;
    for _ in 0..40 {
        if let Ok(c) = FetchClient::connect(format!("http://{addr}")).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut client = client.expect("server didn't accept Fetch");

    let resp = client
        .block(SingleBlockRequest {
            transforms: vec![],
            reference: Some(Reference::BlockNumber(BlockNumber { num: 42 })),
        })
        .await
        .expect("fetch ok despite oracle returning None")
        .into_inner();
    let md = resp.metadata.expect("metadata present");
    assert_eq!(md.num, 42);
    let any = resp.block.expect("block payload present");
    assert_eq!(any.type_url, BLOCK_TYPE_URL);
    let decoded = tron_v1::Block::decode(any.value.as_slice()).expect("decode pb::Block");
    assert_eq!(decoded.number, 42);
    assert_eq!(
        decoded.finality.unwrap().tier,
        tron_v1::FinalityTier::Finalized as i32
    );

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn backfill_below_archive_floor_returns_failed_precondition() {
    // Archive starts at height 100; cache starts at height 110.
    // Subscribe with start_block_num=50 — below archive floor.
    let dir = tempfile::tempdir().unwrap();
    let archive = lightcycle_store::BlockArchive::open(dir.path().join("a.redb")).unwrap();
    for h in 100..105u64 {
        let pb = tron_v1::Block {
            number: h,
            id: vec![h as u8; 32],
            parent_id: vec![(h - 1) as u8; 32],
            time: None,
            header: None,
            transactions: vec![],
            finality: Some(tron_v1::Finality {
                tier: tron_v1::FinalityTier::Finalized as i32,
                solidified_head_number: 200,
            }),
        };
        archive
            .put(h, BlockId([h as u8; 32]), &pb.encode_to_vec())
            .unwrap();
    }
    let cache = lightcycle_store::new_shared::<BufferedBlock>(16);
    {
        let mut g = cache.write().await;
        for h in 110..113u64 {
            let buffered = BufferedBlock {
                height: h,
                block_id: BlockId([h as u8; 32]),
                parent_id: BlockId([(h - 1) as u8; 32]),
                fork_id: 0,
                decoded: DecodedBlock {
                    header: DecodedHeader {
                        height: h,
                        block_id: BlockId([h as u8; 32]),
                        parent_id: BlockId([(h - 1) as u8; 32]),
                        raw_data_hash: [0u8; 32],
                        tx_trie_root: [0u8; 32],
                        timestamp_ms: 0,
                        witness: Address([0x41; 21]),
                        witness_signature: vec![],
                        version: 34,
                    },
                    transactions: vec![],
                },
                tx_infos: vec![],
            };
            g.insert(h, buffered.block_id, buffered);
        }
    }

    let hub = Hub::new(64);
    let (_head_tx, head_rx) =
        tokio::sync::watch::channel::<Option<lightcycle_types::BlockHeight>>(None);
    let backfill = lightcycle_firehose::StreamBackfill {
        cache,
        solidified_head: head_rx,
        archive: Some(archive),
    };
    let addr = pick_addr().await;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let hub = hub.clone();
        async move {
            let _ = serve(
                addr,
                hub,
                empty_oracle(),
                "tron-test",
                Some(backfill),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
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

    // Resume from 50 — both below archive (100) and cache (110).
    // The current implementation walks the archive starting at 50;
    // the archive returns no rows for [50, 99] (range scan finds
    // nothing); then the cache walker discovers cache_walk_from=50
    // is below cache.min_height=110 and returns FailedPrecondition.
    let err = client
        .blocks(Request {
            start_block_num: 50,
            cursor: String::new(),
            stop_block_num: 0,
            final_blocks_only: false,
            transforms: vec![],
        })
        .await
        .expect_err("expected FailedPrecondition");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}
