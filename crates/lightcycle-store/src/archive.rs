//! Persistent block archive — the durable counterpart to [`BlockCache`].
//!
//! ## What it solves
//!
//! [`BlockCache`] holds the last N blocks in memory. Past N (default
//! ~1h of mainnet at 3-second slot times), evicted blocks are gone — a
//! consumer that disconnects for longer than the in-memory window
//! cannot resume via `Stream.Blocks` backfill.
//!
//! `BlockArchive` is the redb-backed durable layer that catches what
//! the cache evicts (or what the relayer explicitly archives on
//! `Output::Irreversible`). Backfill order from the firehose:
//!
//! 1. Walk in-memory cache forward from the consumer's resume height.
//! 2. If the resume height is below `cache.min_height`, walk the
//!    archive forward until catching the cache's lower bound, then
//!    chain into the cache walk.
//! 3. If the resume height is below `archive.min_height`, the
//!    `Stream.Blocks` call returns `FailedPrecondition` — backfill is
//!    not available beyond the operator-configured retention window.
//!
//! ## Schema
//!
//! Single redb table `blocks` mapping `u64` (block height) to
//! `Vec<u8>` of the form `[block_id: 32B][payload: ...]`. Payload is
//! opaque to this layer — the firehose stores `pb::Block`-encoded
//! bytes there, but the archive doesn't know or care.
//!
//! Why a single table with a packed value rather than two tables (one
//! for `block_id`, one for `payload`): redb commits are per-transaction,
//! so two-table puts cost an extra tx or force the caller into open
//! transaction lifecycles. A 32-byte prefix on every value is a
//! 0.05% overhead at typical TRON block sizes (~50 KB) and avoids
//! both complexities.
//!
//! ## What the archive is NOT
//!
//! - **Not a generic kv store.** Only height-keyed reads are
//!   supported. A by-id index would double the on-disk footprint and
//!   is solvable upstream by the cache (which is height + id) for
//!   the recent window, plus the chain itself for ancient blocks.
//! - **Not a cross-replica consistency primitive.** Per ADR-0021,
//!   only chain finality is a legal cross-replica truth. The archive
//!   is a per-replica retention layer; replicas MUST NOT reconcile
//!   their archives against each other. Each replica archives the
//!   blocks its own broadcast saw cross the irreversible threshold.
//! - **Not the engine's reorg-buffer.** Only blocks past the chain's
//!   solidified-head transition land here. By construction these
//!   never get UNDO'd, so the archive is append-only on the happy path.
//!
//! ## Crash semantics
//!
//! Same as [`crate::CursorStore`]: redb is a single-file ACID store,
//! writes are durable on commit, a crash mid-write rolls back to the
//! previous committed state. The archive writer commits per put;
//! callers that want batched writes should call `put_batch`.

use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use thiserror::Error;

use lightcycle_types::{BlockHeight, BlockId};

const BLOCKS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");

/// Errors surfaced by the block archive. Boxes the redb error variants
/// for the same reason [`crate::CursorStoreError`] does — keeps
/// `Result<T, ArchiveError>` small enough to not bloat call frames.
#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("redb open error: {0}")]
    Open(#[source] Box<redb::DatabaseError>),
    #[error("redb transaction error: {0}")]
    Transaction(#[source] Box<redb::TransactionError>),
    #[error("redb table error: {0}")]
    Table(#[source] Box<redb::TableError>),
    #[error("redb storage error: {0}")]
    Storage(#[source] Box<redb::StorageError>),
    #[error("redb commit error: {0}")]
    Commit(#[source] Box<redb::CommitError>),
    #[error(
        "archive value at height {height} is shorter than 32-byte block id prefix ({len} bytes)"
    )]
    Truncated { height: BlockHeight, len: usize },
}

pub(crate) type Result<T> = std::result::Result<T, ArchiveError>;

impl From<redb::DatabaseError> for ArchiveError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Open(Box::new(e))
    }
}
impl From<redb::TransactionError> for ArchiveError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Transaction(Box::new(e))
    }
}
impl From<redb::TableError> for ArchiveError {
    fn from(e: redb::TableError) -> Self {
        Self::Table(Box::new(e))
    }
}
impl From<redb::StorageError> for ArchiveError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(Box::new(e))
    }
}
impl From<redb::CommitError> for ArchiveError {
    fn from(e: redb::CommitError) -> Self {
        Self::Commit(Box::new(e))
    }
}

/// Persistent block archive. Cheap to clone — wraps the inner
/// `Database` in an `Arc` so a single open handle can be shared across
/// the archiver task (writer) and the firehose backfill walker (reader).
/// redb itself handles internal locking so concurrent reads + a single
/// writer Just Work.
#[derive(Debug, Clone)]
pub struct BlockArchive {
    db: Arc<Database>,
}

impl BlockArchive {
    /// Open or create the archive at `path`. Existing data is
    /// preserved across opens; the file is created with the table
    /// initialized so subsequent reads don't surface "table not found."
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(BLOCKS_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Put one block. `payload` is opaque to this layer; the firehose
    /// stores `pb::Block`-encoded bytes here. Overwrites any prior
    /// entry at `height` (the relayer drives writes only for finalized
    /// blocks, so a same-height collision implies the prior write was
    /// for the same block — idempotent).
    pub fn put(&self, height: BlockHeight, block_id: BlockId, payload: &[u8]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BLOCKS_TABLE)?;
            let value = pack_value(block_id, payload);
            table.insert(height, value.as_slice())?;
        }
        txn.commit()?;
        metrics::counter!("lightcycle_store_archive_writes_total").increment(1);
        Ok(())
    }

    /// Atomically write a batch. One commit per call instead of one
    /// per block; useful when the archiver task is catching up after
    /// downtime. Empty batch is a no-op.
    pub fn put_batch(&self, items: &[(BlockHeight, BlockId, Vec<u8>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BLOCKS_TABLE)?;
            for (height, block_id, payload) in items {
                let value = pack_value(*block_id, payload);
                table.insert(*height, value.as_slice())?;
            }
        }
        txn.commit()?;
        metrics::counter!("lightcycle_store_archive_writes_total").increment(items.len() as u64);
        Ok(())
    }

    /// Look up a single block by height. `Ok(None)` is "not in the
    /// archive" (either never written, or below the retention floor).
    pub fn get(&self, height: BlockHeight) -> Result<Option<(BlockId, Vec<u8>)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOCKS_TABLE)?;
        let Some(v) = table.get(height)? else {
            return Ok(None);
        };
        let bytes = v.value().to_vec();
        let (id, payload) = unpack_value(height, &bytes)?;
        Ok(Some((id, payload)))
    }

    /// Inclusive range scan, returned in ascending height order. The
    /// firehose backfill walker uses this to materialize the
    /// pre-cache portion of the resume window in one shot.
    ///
    /// Bounded by `limit`; callers should pick a value that fits in
    /// memory comfortably (a few thousand TRON blocks is well under
    /// 1 GB at typical sizes).
    pub fn range(
        &self,
        start: BlockHeight,
        end_inclusive: BlockHeight,
        limit: usize,
    ) -> Result<Vec<(BlockHeight, BlockId, Vec<u8>)>> {
        if start > end_inclusive || limit == 0 {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOCKS_TABLE)?;
        let mut out = Vec::new();
        let iter = table.range(start..=end_inclusive)?;
        for entry in iter {
            let (k, v) = entry?;
            let height = k.value();
            let bytes = v.value();
            let (id, payload) = unpack_value(height, bytes)?;
            out.push((height, id, payload));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Drop every entry strictly below `floor`. Returns the number of
    /// rows removed. Used by the retention policy task — operator
    /// configures "keep last N days of archive," the task computes the
    /// floor block height once a minute and calls this.
    pub fn delete_below(&self, floor: BlockHeight) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let removed;
        {
            let mut table = txn.open_table(BLOCKS_TABLE)?;
            let to_remove: Vec<u64> = {
                let iter = table.range(..floor)?;
                let mut keys = Vec::new();
                for entry in iter {
                    let (k, _) = entry?;
                    keys.push(k.value());
                }
                keys
            };
            removed = to_remove.len();
            for k in to_remove {
                table.remove(k)?;
            }
        }
        txn.commit()?;
        if removed > 0 {
            metrics::counter!("lightcycle_store_archive_deletes_total").increment(removed as u64);
        }
        Ok(removed)
    }

    /// Lowest archived height, or `None` if the archive is empty.
    /// Used by the firehose backfill walker to short-circuit a request
    /// for a height below the retention floor with `FailedPrecondition`.
    pub fn min_height(&self) -> Result<Option<BlockHeight>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOCKS_TABLE)?;
        let out = {
            let mut iter = table.iter()?;
            match iter.next() {
                Some(entry) => Some(entry?.0.value()),
                None => None,
            }
        };
        Ok(out)
    }

    /// Highest archived height, or `None` if the archive is empty.
    pub fn max_height(&self) -> Result<Option<BlockHeight>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOCKS_TABLE)?;
        let out = {
            let iter = table.iter()?;
            let mut last: Option<BlockHeight> = None;
            for entry in iter {
                last = Some(entry?.0.value());
            }
            last
        };
        Ok(out)
    }

    /// Number of archived blocks. O(table scan) — diagnostics only.
    pub fn len(&self) -> Result<usize> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(BLOCKS_TABLE)?;
        Ok(table.len()? as usize)
    }

    /// Convenience for `len()? == 0`.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

fn pack_value(block_id: BlockId, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(32 + payload.len());
    v.extend_from_slice(&block_id.0);
    v.extend_from_slice(payload);
    v
}

fn unpack_value(height: BlockHeight, bytes: &[u8]) -> Result<(BlockId, Vec<u8>)> {
    if bytes.len() < 32 {
        return Err(ArchiveError::Truncated {
            height,
            len: bytes.len(),
        });
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[..32]);
    let payload = bytes[32..].to_vec();
    Ok((BlockId(id), payload))
}

/// Describe the archive metrics so they show up in Prometheus output
/// before the first write.
pub fn describe_archive_metrics() {
    metrics::describe_counter!(
        "lightcycle_store_archive_writes_total",
        "Block archive put operations (one per finalized block written by the archiver task)."
    );
    metrics::describe_counter!(
        "lightcycle_store_archive_deletes_total",
        "Block archive delete operations (retention-policy pruning)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn id(byte: u8) -> BlockId {
        BlockId([byte; 32])
    }

    #[test]
    fn put_get_round_trip() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        arc.put(100, id(0xa), b"hello").unwrap();
        let got = arc.get(100).unwrap();
        assert_eq!(got, Some((id(0xa), b"hello".to_vec())));
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        assert_eq!(arc.get(999).unwrap(), None);
    }

    #[test]
    fn put_overwrites_prior_entry() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        arc.put(100, id(0xa), b"v1").unwrap();
        arc.put(100, id(0xa), b"v2").unwrap();
        let (_, payload) = arc.get(100).unwrap().unwrap();
        assert_eq!(payload, b"v2");
        assert_eq!(arc.len().unwrap(), 1);
    }

    #[test]
    fn put_batch_commits_atomically() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        let batch: Vec<_> = (100..=104)
            .map(|h| (h, id(h as u8), format!("blk{h}").into_bytes()))
            .collect();
        arc.put_batch(&batch).unwrap();
        assert_eq!(arc.len().unwrap(), 5);
        let (_, p) = arc.get(102).unwrap().unwrap();
        assert_eq!(p, b"blk102");
    }

    #[test]
    fn put_batch_empty_is_noop() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        arc.put_batch(&[]).unwrap();
        assert!(arc.is_empty().unwrap());
    }

    #[test]
    fn range_returns_inclusive_ascending() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        for h in 100..=110 {
            arc.put(h, id(h as u8), format!("blk{h}").as_bytes())
                .unwrap();
        }
        let rows = arc.range(102, 105, 100).unwrap();
        assert_eq!(rows.len(), 4);
        let heights: Vec<_> = rows.iter().map(|r| r.0).collect();
        assert_eq!(heights, vec![102, 103, 104, 105]);
    }

    #[test]
    fn range_respects_limit() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        for h in 100..=110 {
            arc.put(h, id(h as u8), b"x").unwrap();
        }
        let rows = arc.range(100, 110, 3).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, 100);
        assert_eq!(rows[2].0, 102);
    }

    #[test]
    fn range_with_start_after_end_returns_empty() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        arc.put(100, id(0xa), b"x").unwrap();
        let rows = arc.range(101, 100, 100).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn delete_below_drops_strictly_lower_heights() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        for h in 100..=104 {
            arc.put(h, id(h as u8), b"x").unwrap();
        }
        let removed = arc.delete_below(102).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(arc.len().unwrap(), 3);
        assert_eq!(arc.get(101).unwrap(), None);
        assert!(arc.get(102).unwrap().is_some());
    }

    #[test]
    fn min_max_track_inserts_and_deletes() {
        let dir = tempdir().unwrap();
        let arc = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        assert_eq!(arc.min_height().unwrap(), None);
        assert_eq!(arc.max_height().unwrap(), None);
        arc.put(100, id(1), b"x").unwrap();
        arc.put(105, id(5), b"x").unwrap();
        arc.put(110, id(10), b"x").unwrap();
        assert_eq!(arc.min_height().unwrap(), Some(100));
        assert_eq!(arc.max_height().unwrap(), Some(110));
        arc.delete_below(106).unwrap();
        assert_eq!(arc.min_height().unwrap(), Some(110));
        assert_eq!(arc.max_height().unwrap(), Some(110));
    }

    #[test]
    fn data_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.redb");
        {
            let arc = BlockArchive::open(&path).unwrap();
            arc.put(100, id(0xa), b"durable").unwrap();
        }
        let arc = BlockArchive::open(&path).unwrap();
        let (got_id, payload) = arc.get(100).unwrap().unwrap();
        assert_eq!(got_id, id(0xa));
        assert_eq!(payload, b"durable");
    }

    #[test]
    fn cloned_handles_share_one_database() {
        let dir = tempdir().unwrap();
        let a = BlockArchive::open(dir.path().join("a.redb")).unwrap();
        let b = a.clone();
        a.put(100, id(0xa), b"shared").unwrap();
        assert!(b.get(100).unwrap().is_some());
    }

    #[test]
    fn truncated_value_surfaces_typed_error() {
        // Direct redb path: write a too-short value and observe the
        // unpack error. Guards against silent data corruption.
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.redb");
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table(BLOCKS_TABLE).unwrap();
                table.insert(100u64, b"short".as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        let arc = BlockArchive::open(&path).unwrap();
        let err = arc.get(100).unwrap_err();
        match err {
            ArchiveError::Truncated { height, len } => {
                assert_eq!(height, 100);
                assert_eq!(len, 5);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }
}
