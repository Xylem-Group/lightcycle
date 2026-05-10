//! Persistent cursor store for `Stream.Blocks` consumers.
//!
//! ## What it solves
//!
//! Consumers subscribe to `Stream.Blocks` with an opaque [`Cursor`]
//! that encodes `{height, blockId, forkId}`. After a clean shutdown
//! a consumer can resume from the cursor it last received. After a
//! relayer restart, the broadcast hub is empty — and a consumer that
//! reconnects has no way to learn what cursor it was at unless it
//! kept its own state.
//!
//! `CursorStore` is the relayer-side complement: a `redb`-backed
//! map of `consumer_id → Cursor` that the relayer persists on every
//! emission (or on a checkpoint cadence — caller's choice). On
//! reconnect the relayer can ask "what was consumer X at?" and
//! either resume the stream from that cursor (when backfill via the
//! BlockCache covers the gap) or reject the resume with a precise
//! error if the gap exceeds the cache.
//!
//! ## Schema
//!
//! Single redb table `cursors` mapping `&str` (consumer id) → `&[u8]`
//! (the cursor bytes). One key per consumer. The cursor format is
//! the same `(height: u64 BE)(block_id: 32B)(fork_id: u32 BE)` that
//! [`Cursor`] uses on the wire — so the store is binary-compatible
//! with what consumers see on `Stream.Blocks` responses.
//!
//! ## Crash semantics
//!
//! redb is a single-file ACID store. Writes are durable on commit;
//! a crash mid-write rolls back to the previous committed state. The
//! relayer's checkpoint cadence determines how much resume freshness
//! we trade for write throughput — the obvious knob is "checkpoint
//! every N blocks" or "checkpoint on Irreversible." Either is fine;
//! both are caller policy.
//!
//! ## What this is NOT
//!
//! - Not a distributed-state primitive. ADR-0021 is explicit: only
//!   chain finality is a legal cross-replica consistency source.
//!   Two replicas of `lightcycle-store` MUST NOT reconcile their
//!   cursor stores against each other; each replica's CursorStore
//!   tracks the consumers attached to that replica.
//! - Not the durable block archive. Block bytes don't live here;
//!   only the consumer-side resume marker.
//! - Not a substitute for the in-memory [`crate::BlockCache`]. The
//!   cursor tells the relayer where to resume from; the actual
//!   block bytes for that resume window come from the cache (and,
//!   later, from a finalized-block archive the cache spills into).

use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use thiserror::Error;

use lightcycle_types::Cursor;

const CURSORS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("cursors");

/// Errors surfaced by the cursor store. Mostly redb pass-through; the
/// transport-layer wrappers want a typed surface so they don't depend
/// on `redb` directly.
///
/// The redb error variants are large (~160 bytes); we box them so this
/// enum stays small and `Result<T, CursorStoreError>` doesn't bloat
/// every call frame.
#[derive(Debug, Error)]
pub enum CursorStoreError {
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
}

pub(crate) type Result<T> = std::result::Result<T, CursorStoreError>;

// Conversion helpers so the call sites can stay readable
// (`.map_err(Into::into)` instead of explicit boxing).
impl From<redb::DatabaseError> for CursorStoreError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Open(Box::new(e))
    }
}
impl From<redb::TransactionError> for CursorStoreError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Transaction(Box::new(e))
    }
}
impl From<redb::TableError> for CursorStoreError {
    fn from(e: redb::TableError) -> Self {
        Self::Table(Box::new(e))
    }
}
impl From<redb::StorageError> for CursorStoreError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(Box::new(e))
    }
}
impl From<redb::CommitError> for CursorStoreError {
    fn from(e: redb::CommitError) -> Self {
        Self::Commit(Box::new(e))
    }
}

/// Persistent map of `consumer_id → Cursor`. Cheap to clone (the
/// inner `Database` is wrapped in `Arc`), so the relayer can hand
/// references to multiple writers without serializing through one
/// owner. redb itself handles internal locking.
#[derive(Debug, Clone)]
pub struct CursorStore {
    db: Arc<Database>,
}

impl CursorStore {
    /// Open or create the store at `path`. The file is created
    /// if missing; existing data is preserved across opens.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path)?;
        // Initialize the table on first open so subsequent reads
        // don't surface "table not found" (redb requires the table
        // to exist; opening for write creates it idempotently).
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(CURSORS_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Persist a cursor for `consumer_id`. Overwrites any prior
    /// cursor for the same id. Commits synchronously — the call
    /// returns only after the write is durable on disk. Hot-path
    /// callers should consider checkpointing (call once every N
    /// blocks) rather than per-block to keep redb's
    /// fsync overhead off the critical path.
    pub fn put(&self, consumer_id: &str, cursor: &Cursor) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CURSORS_TABLE)?;
            table.insert(consumer_id, cursor.0.as_slice())?;
        }
        txn.commit()?;
        metrics::counter!("lightcycle_store_cursor_writes_total").increment(1);
        Ok(())
    }

    /// Look up a consumer's cursor, if any. `Ok(None)` is the
    /// "no prior cursor for this consumer" state — consumers that
    /// have never connected before, or whose entry was explicitly
    /// deleted.
    pub fn get(&self, consumer_id: &str) -> Result<Option<Cursor>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CURSORS_TABLE)?;
        let bytes = table.get(consumer_id)?.map(|v| v.value().to_vec());
        Ok(bytes.map(Cursor))
    }

    /// Delete a consumer's stored cursor. Idempotent — deleting a
    /// non-existent entry returns `Ok(false)`.
    pub fn delete(&self, consumer_id: &str) -> Result<bool> {
        let txn = self.db.begin_write()?;
        let removed;
        {
            let mut table = txn.open_table(CURSORS_TABLE)?;
            removed = table.remove(consumer_id)?.is_some();
        }
        txn.commit()?;
        if removed {
            metrics::counter!("lightcycle_store_cursor_deletes_total").increment(1);
        }
        Ok(removed)
    }

    /// Enumerate every (consumer_id, cursor) pair. Cheap snapshot —
    /// the read transaction sees a consistent view at `list()` call
    /// time. Used by the ops dashboard to show "which consumer is at
    /// which height" without poking each consumer.
    pub fn list(&self) -> Result<Vec<(String, Cursor)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CURSORS_TABLE)?;
        let mut out = Vec::new();
        let iter = table.iter()?;
        for entry in iter {
            let (k, v) = entry?;
            out.push((k.value().to_string(), Cursor(v.value().to_vec())));
        }
        Ok(out)
    }

    /// Number of distinct consumer ids currently tracked. O(table
    /// scan) — fine for the small consumer counts we expect (tens to
    /// hundreds), don't call on a hot path.
    pub fn len(&self) -> Result<usize> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CURSORS_TABLE)?;
        let len = table.len()? as usize;
        Ok(len)
    }

    /// Convenience for `len() == 0` without an extra transaction
    /// allocation in callers' eyes — paired with `len()` so clippy
    /// stops complaining about the missing partner method.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Describe the cursor-store metrics so they show up in Prometheus
/// output before the first write.
pub fn describe_cursor_store_metrics() {
    metrics::describe_counter!(
        "lightcycle_store_cursor_writes_total",
        "Cursor-store put operations (one per checkpoint persistence)."
    );
    metrics::describe_counter!(
        "lightcycle_store_cursor_deletes_total",
        "Cursor-store delete operations (consumer eviction or operator action)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cur(bytes: &[u8]) -> Cursor {
        Cursor(bytes.to_vec())
    }

    #[test]
    fn put_get_round_trip() {
        let dir = tempdir().unwrap();
        let store = CursorStore::open(dir.path().join("cursors.redb")).unwrap();
        store.put("alice", &cur(b"hello")).unwrap();
        let got = store.get("alice").unwrap();
        assert_eq!(got, Some(cur(b"hello")));
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempdir().unwrap();
        let store = CursorStore::open(dir.path().join("cursors.redb")).unwrap();
        assert_eq!(store.get("ghost").unwrap(), None);
    }

    #[test]
    fn put_overwrites_prior_cursor() {
        let dir = tempdir().unwrap();
        let store = CursorStore::open(dir.path().join("cursors.redb")).unwrap();
        store.put("alice", &cur(b"v1")).unwrap();
        store.put("alice", &cur(b"v2")).unwrap();
        assert_eq!(store.get("alice").unwrap(), Some(cur(b"v2")));
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn delete_returns_true_when_present_false_when_missing() {
        let dir = tempdir().unwrap();
        let store = CursorStore::open(dir.path().join("cursors.redb")).unwrap();
        store.put("alice", &cur(b"x")).unwrap();
        assert!(store.delete("alice").unwrap());
        assert!(!store.delete("alice").unwrap());
        assert_eq!(store.get("alice").unwrap(), None);
    }

    #[test]
    fn list_enumerates_every_entry() {
        let dir = tempdir().unwrap();
        let store = CursorStore::open(dir.path().join("cursors.redb")).unwrap();
        store.put("alice", &cur(b"a")).unwrap();
        store.put("bob", &cur(b"b")).unwrap();
        store.put("eve", &cur(b"e")).unwrap();
        let mut all = store.list().unwrap();
        all.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], ("alice".to_string(), cur(b"a")));
        assert_eq!(all[1], ("bob".to_string(), cur(b"b")));
        assert_eq!(all[2], ("eve".to_string(), cur(b"e")));
    }

    #[test]
    fn data_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cursors.redb");
        {
            let store = CursorStore::open(&path).unwrap();
            store.put("alice", &cur(b"durable")).unwrap();
        }
        // Drop + reopen.
        let store = CursorStore::open(&path).unwrap();
        assert_eq!(store.get("alice").unwrap(), Some(cur(b"durable")));
    }

    #[test]
    fn cloned_handles_share_one_database() {
        // Cheap-to-clone: two handles wrap the same Arc<Database>;
        // a write through one is visible through the other.
        let dir = tempdir().unwrap();
        let store_a = CursorStore::open(dir.path().join("cursors.redb")).unwrap();
        let store_b = store_a.clone();
        store_a.put("alice", &cur(b"cloned")).unwrap();
        assert_eq!(store_b.get("alice").unwrap(), Some(cur(b"cloned")));
    }
}
