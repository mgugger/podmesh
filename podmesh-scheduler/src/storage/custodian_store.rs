//! Custodian store — persists DEK shares and custodian records for a given owner.
//!
//! Each custodian node that accepts custody of a manifest stores one record per
//! manifest_id. Records are replicated to peer custodians via gossipsub (Phase 2.4).
//!
//! **Schema**
//! - Table `custodian_records`: `manifest_id (&str)` → `CustodianRecord (postcard bytes)`
//!
//! The DEK share bytes themselves are stored `Zeroizing`-wrapped in memory only; only
//! the **wrapped** (KEM-encrypted) form is persisted to disk so the raw share is never
//! written in plaintext.

use log::{debug, info, warn};
use once_cell::sync::OnceCell;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

const CUSTODIAN_RECORDS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("custodian_records");

static GLOBAL_CUSTODIAN_STORE: OnceCell<Arc<CustodianStore>> = OnceCell::new();

/// Errors from the custodian store.
#[derive(Error, Debug)]
pub enum CustodianStoreError {
    #[error("Database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Not initialized")]
    NotInitialized,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CustodianStoreError>;

// ---------------------------------------------------------------------------
// Record types
// ---------------------------------------------------------------------------

/// Persistent record for a single manifest custody assignment.
///
/// `wrapped_share` stores the DEK share encrypted to *this custodian node's*
/// KEM public key so it can be recovered after a restart.  When a worker
/// requests its share, the custodian decrypts this blob, re-encrypts it to the
/// worker's KEM pubkey, and sends it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustodianRecord {
    /// blake3 manifest ID (hex string)
    pub manifest_id: String,
    /// Ed25519 pubkey of the workload owner (base64)
    pub owner_pubkey: String,
    /// Total number of shares created (n)
    pub shares_total: u8,
    /// Minimum shares required to reconstruct (k / threshold)
    pub shares_threshold: u8,
    /// This node's share index (1-based)
    pub share_index: u8,
    /// DEK share bytes, wrapped (KEM-encrypted) to this node's own KEM pubkey.
    /// Raw bytes are never stored in plaintext.
    pub wrapped_share: Vec<u8>,
    /// Peer IDs of all custodians for this manifest (for coordinator election)
    pub custodian_peers: Vec<String>,
    /// Unix epoch milliseconds when this record was created
    pub created_at_ms: u64,
    /// Unix epoch milliseconds of the last replication update
    pub updated_at_ms: u64,
    /// Base64 Ed25519 signing pubkey of the coordinator that assigned this workload.
    /// Used to verify assignment tokens in worker `ShareRequest`s.
    #[serde(default)]
    pub coordinator_pubkey: String,
    /// libp2p PeerId (base58) of the custodian that owns this record.
    /// Used to disambiguate records when multiple custodians share a process-global store.
    #[serde(default)]
    pub local_peer_id: String,
}

impl CustodianRecord {
    pub fn new(
        manifest_id: String,
        owner_pubkey: String,
        shares_total: u8,
        shares_threshold: u8,
        share_index: u8,
        wrapped_share: Vec<u8>,
        custodian_peers: Vec<String>,
    ) -> Self {
        let now = now_ms();
        Self {
            manifest_id,
            owner_pubkey,
            shares_total,
            shares_threshold,
            share_index,
            wrapped_share,
            custodian_peers,
            created_at_ms: now,
            updated_at_ms: now,
            coordinator_pubkey: String::new(),
            local_peer_id: String::new(),
        }
    }

    pub fn with_coordinator_pubkey(mut self, coordinator_pubkey: String) -> Self {
        self.coordinator_pubkey = coordinator_pubkey;
        self
    }

    pub fn with_local_peer_id(mut self, local_peer_id: String) -> Self {
        self.local_peer_id = local_peer_id;
        self
    }

    /// Returns the store key: `{manifest_id}:{local_peer_id}` if `local_peer_id` is set,
    /// otherwise `{manifest_id}` for backward compatibility.
    pub fn store_key(&self) -> String {
        if self.local_peer_id.is_empty() {
            self.manifest_id.clone()
        } else {
            format!("{}:{}", self.manifest_id, self.local_peer_id)
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Persistent custodian store backed by a redb database.
pub struct CustodianStore {
    db: Database,
}

impl CustodianStore {
    /// Open (or create) the store at `db_path`.
    pub fn open(db_path: &PathBuf) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        info!("Opening custodian store at {:?}", db_path);
        let db = Database::create(db_path)?;

        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
        }
        write_txn.commit()?;

        Ok(Self { db })
    }

    /// Create an ephemeral (temp-file-backed) store for tests.
    pub fn open_ephemeral() -> Result<Self> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("custodian_test_{}.redb", ts));
        Self::open(&path)
    }

    // --- write operations ---

    /// Upsert a custodian record. The record is keyed by `store_key()` which
    /// is `{manifest_id}:{local_peer_id}` when `local_peer_id` is set, or
    /// just `{manifest_id}` for backward compatibility.
    pub fn set_record(&self, record: &CustodianRecord) -> Result<()> {
        let bytes = postcard::to_allocvec(record)
            .map_err(|e| CustodianStoreError::Serialization(e.to_string()))?;
        let key = record.store_key();
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
            table.insert(key.as_str(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        debug!("custodian_store: upserted record for {}", key);
        Ok(())
    }

    /// Remove the record for `manifest_id` (e.g. workload deleted).
    /// Removes ALL records whose key starts with `{manifest_id}:` or equals `{manifest_id}`.
    pub fn remove_record(&self, manifest_id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let mut removed = false;
        {
            let mut table = write_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
            // Remove exact-match key (backward compat).
            if table.remove(manifest_id)?.is_some() {
                removed = true;
            }
            // Remove all peer-scoped keys.
            let prefix = format!("{}:", manifest_id);
            let keys_to_remove: Vec<String> = table
                .iter()?
                .filter_map(|e| e.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| k.starts_with(&prefix))
                .collect();
            for key in keys_to_remove {
                if table.remove(key.as_str())?.is_some() {
                    removed = true;
                }
            }
        }
        write_txn.commit()?;
        if removed {
            debug!("custodian_store: removed record(s) for {}", manifest_id);
        }
        Ok(removed)
    }

    /// Update only the `custodian_peers` list for the given manifest and peer.
    pub fn update_peers(&self, manifest_id: &str, peers: Vec<String>) -> Result<()> {
        // Try peer-scoped key first; fall back to bare manifest_id.
        let mut record = self
            .get_record(manifest_id)?
            .ok_or_else(|| CustodianStoreError::Serialization(format!("no record for {manifest_id}")))?;
        record.custodian_peers = peers;
        record.updated_at_ms = now_ms();
        self.set_record(&record)
    }

    // --- read operations ---

    /// Get the record for `manifest_id`.
    /// Returns the first record whose key equals `{manifest_id}` or starts with `{manifest_id}:`.
    pub fn get_record(&self, manifest_id: &str) -> Result<Option<CustodianRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
        // Exact match first (backward compat).
        if let Some(bytes) = table.get(manifest_id)? {
            let record = postcard::from_bytes(bytes.value())
                .map_err(|e| CustodianStoreError::Serialization(e.to_string()))?;
            return Ok(Some(record));
        }
        // Scan for peer-scoped key.
        let prefix = format!("{}:", manifest_id);
        for entry in table.iter()? {
            let (k, v) = entry?;
            if k.value().starts_with(&prefix) {
                let record: CustodianRecord = postcard::from_bytes(v.value())
                    .map_err(|e| CustodianStoreError::Serialization(e.to_string()))?;
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// Get the record for `manifest_id` belonging to `peer_id`.
    /// Uses key `{manifest_id}:{peer_id}` if `peer_id` is non-empty,
    /// otherwise falls back to bare `{manifest_id}`.
    pub fn get_record_for_peer(&self, manifest_id: &str, peer_id: &str) -> Result<Option<CustodianRecord>> {
        if peer_id.is_empty() {
            return self.get_record(manifest_id);
        }
        let key = format!("{}:{}", manifest_id, peer_id);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
        match table.get(key.as_str())? {
            None => Ok(None),
            Some(bytes) => {
                let record = postcard::from_bytes(bytes.value())
                    .map_err(|e| CustodianStoreError::Serialization(e.to_string()))?;
                Ok(Some(record))
            }
        }
    }

    /// List all custodian records.
    pub fn list_records(&self) -> Result<Vec<CustodianRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            match postcard::from_bytes::<CustodianRecord>(v.value()) {
                Ok(r) => records.push(r),
                Err(e) => warn!("custodian_store: failed to deserialize record: {}", e),
            }
        }
        Ok(records)
    }

    /// List all records for a given `manifest_id` (all peer-scoped variants).
    pub fn list_records_for_manifest(&self, manifest_id: &str) -> Result<Vec<CustodianRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CUSTODIAN_RECORDS_TABLE)?;
        let prefix = format!("{}:", manifest_id);
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let key = k.value();
            if key == manifest_id || key.starts_with(&prefix) {
                match postcard::from_bytes::<CustodianRecord>(v.value()) {
                    Ok(r) => records.push(r),
                    Err(e) => warn!("custodian_store: failed to deserialize record for {}: {}", key, e),
                }
            }
        }
        Ok(records)
    }

    /// Returns the manifest IDs for which this node holds custody.
    pub fn manifest_ids(&self) -> Result<Vec<String>> {
        Ok(self.list_records()?.into_iter().map(|r| r.manifest_id).collect())
    }
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

/// Initialize the global custodian store.
///
/// - If `ephemeral` is true, opens a temp-file-backed store (for tests / `--ephemeral`).
/// - Otherwise uses `~/.podmesh/custodian.redb`.
///
/// Safe to call multiple times; subsequent calls are no-ops.
pub fn init_custodian_store(ephemeral: bool) -> Result<()> {
    if GLOBAL_CUSTODIAN_STORE.get().is_some() {
        return Ok(());
    }
    let store = if ephemeral {
        CustodianStore::open_ephemeral()?
    } else {
        let path = dirs::home_dir()
            .map(|h| h.join(".podmesh").join("custodian.redb"))
            .unwrap_or_else(|| PathBuf::from("/var/lib/podmesh/custodian.redb"));
        CustodianStore::open(&path)?
    };
    // Ignore set() error — another thread may have raced us.
    let _ = GLOBAL_CUSTODIAN_STORE.set(Arc::new(store));
    Ok(())
}

/// Returns the global custodian store, or `None` if not initialized.
pub fn get_custodian_store() -> Option<Arc<CustodianStore>> {
    GLOBAL_CUSTODIAN_STORE.get().cloned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ephemeral() -> CustodianStore {
        CustodianStore::open_ephemeral().unwrap()
    }

    fn record(manifest_id: &str, index: u8) -> CustodianRecord {
        CustodianRecord::new(
            manifest_id.to_string(),
            "owner_pubkey_base64".to_string(),
            5,
            3,
            index,
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            vec!["peer1".to_string(), "peer2".to_string()],
        )
    }

    #[test]
    fn test_set_and_get_record() {
        let store = ephemeral();
        let r = record("manifest-abc", 1);
        store.set_record(&r).unwrap();
        let got = store.get_record("manifest-abc").unwrap().unwrap();
        assert_eq!(got.manifest_id, "manifest-abc");
        assert_eq!(got.share_index, 1);
        assert_eq!(got.shares_total, 5);
        assert_eq!(got.shares_threshold, 3);
        assert_eq!(got.wrapped_share, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_get_missing_returns_none() {
        let store = ephemeral();
        assert!(store.get_record("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn test_remove_record() {
        let store = ephemeral();
        store.set_record(&record("to-remove", 2)).unwrap();
        assert!(store.remove_record("to-remove").unwrap());
        assert!(store.get_record("to-remove").unwrap().is_none());
        // Idempotent second remove
        assert!(!store.remove_record("to-remove").unwrap());
    }

    #[test]
    fn test_list_records() {
        let store = ephemeral();
        store.set_record(&record("m1", 1)).unwrap();
        store.set_record(&record("m2", 2)).unwrap();
        store.set_record(&record("m3", 3)).unwrap();
        let ids: std::collections::HashSet<_> = store
            .list_records()
            .unwrap()
            .into_iter()
            .map(|r| r.manifest_id)
            .collect();
        assert!(ids.contains("m1"));
        assert!(ids.contains("m2"));
        assert!(ids.contains("m3"));
    }

    #[test]
    fn test_update_peers() {
        let store = ephemeral();
        store.set_record(&record("m-peers", 1)).unwrap();
        store
            .update_peers("m-peers", vec!["new-peer".to_string()])
            .unwrap();
        let got = store.get_record("m-peers").unwrap().unwrap();
        assert_eq!(got.custodian_peers, vec!["new-peer"]);
    }

    #[test]
    fn test_manifest_ids() {
        let store = ephemeral();
        store.set_record(&record("alpha", 1)).unwrap();
        store.set_record(&record("beta", 2)).unwrap();
        let ids = store.manifest_ids().unwrap();
        assert!(ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));
    }
}
