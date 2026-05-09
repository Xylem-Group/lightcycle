//! Broadcast multiplex hub.
//!
//! Bridges the relayer's single-consumer mpsc output channel to a
//! many-consumer broadcast channel that the gRPC `Stream` server
//! subscribes to per-RPC. Slow subscribers `Lagged` rather than
//! back-pressuring the engine — correct semantics for a relayer:
//! one stuck consumer must not stall block ingestion for everyone.
//!
//! Capacity is configurable. The right value is "enough to absorb
//! a slow subscriber's transient blip without dropping": rough rule
//! of thumb = poll_interval × N seconds × max_lag_we_tolerate. For a
//! 1s poll and 5-minute tolerance, capacity = 300 is plenty.

use lightcycle_relayer::Output;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

/// The hub's external surface: clone the sender to subscribe.
#[derive(Clone, Debug)]
pub struct Hub {
    tx: broadcast::Sender<Output>,
}

impl Hub {
    /// Build a hub. `capacity` is the per-subscriber buffer; if a
    /// subscriber falls more than `capacity` behind it gets a
    /// `RecvError::Lagged` on the next call to `recv`.
    pub fn new(capacity: usize) -> Self {
        // We discard the initial receiver — subscribers create their
        // own via `subscribe()`. With zero receivers, broadcast::send
        // returns an `Err` indicating no consumers, which we silently
        // ignore in the pump task; it's the expected steady-state
        // before any client connects.
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// New subscriber. Each gRPC `Blocks` RPC call gets one.
    pub fn subscribe(&self) -> broadcast::Receiver<Output> {
        self.tx.subscribe()
    }

    /// How many subscribers are currently attached. Useful for
    /// shutdown logic and metrics.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Pump from the relayer's mpsc into the hub's broadcast.
    /// Spawns a background task that runs until `input` closes.
    /// Returns the join handle so the caller can await shutdown.
    pub fn pump_from(&self, mut input: mpsc::Receiver<Output>) -> tokio::task::JoinHandle<()> {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(output) = input.recv().await {
                // Track active subscribers so we can update a gauge.
                metrics::gauge!("lightcycle_firehose_subscribers")
                    .set(tx.receiver_count() as f64);

                // broadcast::send only errors when there are zero
                // active receivers — that's "nobody listening yet,"
                // not a fatal condition.
                if tx.send(output).is_err() {
                    debug!(
                        "no firehose subscribers; output dropped (steady-state pre-connect)"
                    );
                    metrics::counter!(
                        "lightcycle_firehose_outputs_total",
                        "result" => "dropped_no_subscribers"
                    )
                    .increment(1);
                } else {
                    metrics::counter!(
                        "lightcycle_firehose_outputs_total",
                        "result" => "broadcast"
                    )
                    .increment(1);
                }
            }
            warn!("hub pump: input mpsc closed; exiting");
        })
    }

    /// For tests / metrics — sender clone.
    pub fn sender(&self) -> broadcast::Sender<Output> {
        self.tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_codec::{DecodedBlock, DecodedHeader};
    use lightcycle_relayer::{BufferedBlock, Cursor, StreamableBlock};
    use lightcycle_types::{Address, BlockFinality, BlockId, FinalityTier, Step};
    use std::time::Duration;

    fn synth_output_new(height: u64) -> Output {
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
        Output::New(StreamableBlock {
            step: Step::New,
            cursor: Cursor::new(height, BlockId([height as u8; 32])),
            block,
            finality: BlockFinality {
                tier: FinalityTier::Seen,
                solidified_head: None,
            },
        })
    }

    #[tokio::test]
    async fn pump_propagates_to_subscriber() {
        let hub = Hub::new(8);
        let mut sub = hub.subscribe();
        let (tx, rx) = mpsc::channel(8);
        let _h = hub.pump_from(rx);

        tx.send(synth_output_new(100)).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("recv timeout")
            .expect("recv error");
        match got {
            Output::New(s) => assert_eq!(s.block.height, 100),
            _ => panic!("expected New"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive() {
        let hub = Hub::new(8);
        let mut a = hub.subscribe();
        let mut b = hub.subscribe();
        let (tx, rx) = mpsc::channel(8);
        let _h = hub.pump_from(rx);

        tx.send(synth_output_new(100)).await.unwrap();
        let got_a = tokio::time::timeout(Duration::from_millis(500), a.recv())
            .await
            .unwrap()
            .unwrap();
        let got_b = tokio::time::timeout(Duration::from_millis(500), b.recv())
            .await
            .unwrap()
            .unwrap();
        match (got_a, got_b) {
            (Output::New(sa), Output::New(sb)) => {
                assert_eq!(sa.block.height, 100);
                assert_eq!(sb.block.height, 100);
            }
            _ => panic!("expected New on both subs"),
        }
    }

    #[tokio::test]
    async fn no_subscribers_doesnt_panic() {
        let hub = Hub::new(8);
        let (tx, rx) = mpsc::channel(8);
        let _h = hub.pump_from(rx);

        // Push without any subscriber — must not panic, must not stall.
        tx.send(synth_output_new(100)).await.unwrap();
        // Pump should still be alive.
        assert_eq!(hub.receiver_count(), 0);
    }
}
