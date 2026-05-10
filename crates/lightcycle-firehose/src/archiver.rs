//! Archiver task: subscribes to the relayer's broadcast and persists
//! every block that crosses the chain's solidified-head threshold
//! (i.e. every `Output::Irreversible`) into a [`BlockArchive`].
//!
//! ## Why a separate task
//!
//! The relayer broadcasts `Output`s through a `tokio::broadcast`
//! channel. Multiple subscribers (the firehose Stream service, this
//! archiver, future Kafka sinks, etc.) attach via `subscribe()` and
//! consume independently. Decoupling the archive write from the
//! relayer's hot path means:
//!
//! - The relayer's tick loop never blocks on disk I/O. If the archive
//!   is slow (e.g. SSD latency spike), the broadcast buffers up to
//!   capacity; on overrun the archiver gets `Lagged` and resyncs from
//!   the next `Irreversible`. The relayer never sees back-pressure.
//! - The archive's encoding contract (`pb::Block` bytes via
//!   [`crate::encode_block`]) is a firehose concern, not a relayer
//!   concern. Co-locating the encoder with the archiver is the right
//!   layering.
//!
//! ## What gets written
//!
//! Only `Output::Irreversible(StreamableBlock)`. By construction:
//!
//! - The block is past the chain's solidified-head threshold per the
//!   engine's view, so it will not be reorganised.
//! - `step` is implicitly `New` (Irreversible is a tier transition,
//!   not a separate step in the wire vocabulary).
//! - `finality.tier == Finalized` and `finality.solidified_head` is
//!   populated.
//!
//! The archive value is `pb::Block.encode_to_vec()` — the same shape
//! `Stream.Blocks` and `Fetch.Block` re-emit on the wire.
//!
//! ## Crash semantics
//!
//! redb commits on every put. A crash mid-write rolls back to the
//! last committed state — at most one block in flight is lost, and
//! the archiver re-derives it from the next `Irreversible` after
//! restart (the relayer re-emits `Irreversible` for any block that
//! crossed the threshold during the gap, by walking its canonical
//! buffer against the chain's solidified head on cold start).
//!
//! Lag handling: `Lagged(n)` is treated as a soft failure; we log
//! and increment a counter but continue. The lost frames will be
//! re-sent next time the chain advances and the engine re-emits
//! Irreversible for any window the archive missed during the gap.

use lightcycle_relayer::Output;
use lightcycle_store::BlockArchive;
use prost::Message;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::encode::encode_block;

/// Run the archiver loop. Returns when the broadcast sender is
/// dropped (i.e. the relayer shuts down). The CLI typically spawns
/// this with `tokio::spawn` alongside the firehose `serve` future.
pub async fn run_archiver(mut rx: broadcast::Receiver<Output>, archive: BlockArchive) {
    debug!("archiver task started");
    metrics::counter!("lightcycle_firehose_archiver_starts_total").increment(1);

    loop {
        match rx.recv().await {
            Ok(Output::Irreversible(sb)) => {
                let height = sb.block.height;
                let block_id = sb.block.block_id;
                // Re-encode pb::Block with the engine's finality
                // claim. The archive then carries a self-describing
                // block with a valid finality envelope, so a backfill
                // emission from the archive doesn't depend on
                // re-fetching the chain state at read time.
                let bytes = encode_block(&sb.block, sb.finality).encode_to_vec();
                match archive.put(height, block_id, &bytes) {
                    Ok(()) => {
                        metrics::counter!("lightcycle_firehose_archiver_writes_total").increment(1);
                        tracing::trace!(height, "archived block");
                    }
                    Err(e) => {
                        warn!(error = ?e, height, "archive write failed; block not persisted");
                        metrics::counter!("lightcycle_firehose_archiver_errors_total").increment(1);
                    }
                }
            }
            Ok(_) => {
                // Other Output variants (New / Undo / ForkObserved /
                // ForkResolved) are not archived. By design — only
                // past-finality blocks land in the archive.
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "archiver lagged the broadcast hub");
                metrics::counter!("lightcycle_firehose_archiver_lagged_total").increment(skipped);
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("archiver: broadcast channel closed; exiting");
                break;
            }
        }
    }
}

/// Describe the archiver metrics so they show up in Prometheus output
/// from process startup.
pub fn describe_archiver_metrics() {
    metrics::describe_counter!(
        "lightcycle_firehose_archiver_starts_total",
        "Block archiver task starts (process restarts)."
    );
    metrics::describe_counter!(
        "lightcycle_firehose_archiver_writes_total",
        "Blocks written to the archive (one per Output::Irreversible)."
    );
    metrics::describe_counter!(
        "lightcycle_firehose_archiver_errors_total",
        "Archive write errors. Sustained nonzero rate means the disk \
         is saturating or the redb file is misconfigured."
    );
    metrics::describe_counter!(
        "lightcycle_firehose_archiver_lagged_total",
        "Total broadcast frames the archiver missed due to Lagged. \
         The relayer re-emits Irreversible on cold start so the \
         archive recovers eventually, but a high rate indicates the \
         broadcast buffer is undersized."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcycle_codec::{DecodedBlock, DecodedHeader};
    use lightcycle_relayer::{BufferedBlock, Cursor, Output, StreamableBlock};
    use lightcycle_types::{Address, BlockFinality, BlockId, FinalityTier, Step};
    use tempfile::tempdir;

    fn synth_irrev(height: u64) -> Output {
        let block = BufferedBlock {
            height,
            block_id: BlockId([height as u8; 32]),
            parent_id: BlockId([(height.wrapping_sub(1)) as u8; 32]),
            fork_id: 0,
            decoded: DecodedBlock {
                header: DecodedHeader {
                    height,
                    block_id: BlockId([height as u8; 32]),
                    parent_id: BlockId([(height.wrapping_sub(1)) as u8; 32]),
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
        Output::Irreversible(StreamableBlock {
            step: Step::Irreversible,
            cursor: Cursor::new(height, BlockId([height as u8; 32])),
            block,
            finality: BlockFinality {
                tier: FinalityTier::Finalized,
                solidified_head: Some(height + 30),
            },
        })
    }

    #[tokio::test]
    async fn archiver_writes_irreversible_blocks() {
        let dir = tempdir().unwrap();
        let archive = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        let (tx, rx) = broadcast::channel(16);
        let archive_handle = archive.clone();
        let task = tokio::spawn(run_archiver(rx, archive_handle));

        for h in 100..=104 {
            tx.send(synth_irrev(h)).expect("send");
        }
        // Drop sender to terminate the loop.
        drop(tx);
        task.await.expect("archiver join");

        assert_eq!(archive.len().unwrap(), 5);
        assert!(archive.get(102).unwrap().is_some());
    }

    #[tokio::test]
    async fn archiver_ignores_non_irreversible_outputs() {
        // Synthesize a New step (not Irreversible) and confirm the
        // archive remains empty.
        let dir = tempdir().unwrap();
        let archive = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        let (tx, rx) = broadcast::channel(16);
        let archive_handle = archive.clone();
        let task = tokio::spawn(run_archiver(rx, archive_handle));

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
                    timestamp_ms: 0,
                    witness: Address([0x41; 21]),
                    witness_signature: vec![],
                    version: 34,
                },
                transactions: vec![],
            },
            tx_infos: vec![],
        };
        let new_evt = Output::New(StreamableBlock {
            step: Step::New,
            cursor: Cursor::new(100, BlockId([100u8; 32])),
            block,
            finality: BlockFinality {
                tier: FinalityTier::Seen,
                solidified_head: None,
            },
        });
        tx.send(new_evt).expect("send");
        drop(tx);
        task.await.expect("archiver join");

        assert!(archive.is_empty().unwrap());
    }
}
