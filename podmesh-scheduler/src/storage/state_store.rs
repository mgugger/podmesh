//! State store implementation using redb for persistent storage.
//!
//! Stores manifest-owner mappings and deployment records for recovery
//! after scheduler restart.

use log::{debug, info, warn};
use once_cell::sync::OnceCell;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

/// Table for manifest_id -> owner_pubkey mapping
const MANIFEST_OWNERS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("manifest_owners");

/// Table for workload_id -> deployment record (JSON serialized)
const DEPLOYMENTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("deployments");

/// Global state store instance
static GLOBAL_STATE_STORE: OnceCell<Arc<StateStore>> = OnceCell::new();

/// Storage errors
#[derive(Error, Debug)]
pub enum StorageError {
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

pub type Result<T> = std::result::Result<T, StorageError>;

/// Configuration for the state store
#[derive(Debug, Clone)]
pub struct StateStoreConfig {
    /// Path to the database file
    pub db_path: PathBuf,
}

impl Default for StateStoreConfig {
    fn default() -> Self {
        // Use ~/.podmesh/state.redb for user mode
        let db_path = dirs::home_dir()
            .map(|h| h.join(".podmesh").join("state.redb"))
            .unwrap_or_else(|| PathBuf::from("/var/lib/podmesh/state.redb"));
        
        Self { db_path }
    }
}

/// Record stored for each deployment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub workload_id: String,
    pub manifest_id: String,
    pub status: String,
    pub runtime_engine: String,
    pub owner_pubkey: Vec<u8>,
    pub deployed_at_ms: u64,
    pub last_updated_ms: u64,
}

impl DeploymentRecord {
    pub fn new(
        workload_id: String,
        manifest_id: String,
        status: String,
        runtime_engine: String,
        owner_pubkey: Vec<u8>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        
        Self {
            workload_id,
            manifest_id,
            status,
            runtime_engine,
            owner_pubkey,
            deployed_at_ms: now,
            last_updated_ms: now,
        }
    }
    
    pub fn with_status(mut self, status: &str) -> Self {
        self.status = status.to_string();
        self.last_updated_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self
    }
}

/// Persistent state store using redb
pub struct StateStore {
    db: Database,
}

impl StateStore {
    /// Open or create a state store at the given path
    pub fn open(config: &StateStoreConfig) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = config.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        info!("Opening state store at {:?}", config.db_path);
        let db = Database::create(&config.db_path)?;
        
        // Initialize tables
        let write_txn = db.begin_write()?;
        {
            // Create tables if they don't exist
            let _ = write_txn.open_table(MANIFEST_OWNERS_TABLE)?;
            let _ = write_txn.open_table(DEPLOYMENTS_TABLE)?;
        }
        write_txn.commit()?;
        
        info!("State store initialized successfully");
        Ok(Self { db })
    }
    
    /// Open with default configuration
    pub fn open_default() -> Result<Self> {
        Self::open(&StateStoreConfig::default())
    }
    
    /// Open an in-memory database for testing
    pub fn open_ephemeral() -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        
        let temp_dir = std::env::temp_dir();
        let unique_id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db_path = temp_dir.join(format!(
            "podmesh_test_{}_{}.redb",
            std::process::id(),
            unique_id
        ));
        let config = StateStoreConfig { db_path };
        Self::open(&config)
    }
    
    // ========== Manifest Owner Operations ==========
    
    /// Store the owner public key for a manifest
    pub fn set_manifest_owner(&self, manifest_id: &str, owner_pubkey: &[u8]) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(MANIFEST_OWNERS_TABLE)?;
            table.insert(manifest_id, owner_pubkey)?;
        }
        write_txn.commit()?;
        debug!("Stored owner for manifest_id={} (pubkey len={})", manifest_id, owner_pubkey.len());
        Ok(())
    }
    
    /// Get the owner public key for a manifest
    pub fn get_manifest_owner(&self, manifest_id: &str) -> Result<Option<Vec<u8>>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(MANIFEST_OWNERS_TABLE)?;
        
        match table.get(manifest_id)? {
            Some(value) => Ok(Some(value.value().to_vec())),
            None => Ok(None),
        }
    }
    
    /// Remove the owner mapping for a manifest
    pub fn remove_manifest_owner(&self, manifest_id: &str) -> Result<Option<Vec<u8>>> {
        let write_txn = self.db.begin_write()?;
        let result = {
            let mut table = write_txn.open_table(MANIFEST_OWNERS_TABLE)?;
            table.remove(manifest_id)?.map(|v| v.value().to_vec())
        };
        write_txn.commit()?;
        Ok(result)
    }
    
    /// List all manifest-owner mappings
    pub fn list_manifest_owners(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(MANIFEST_OWNERS_TABLE)?;
        
        let mut results = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            results.push((key.value().to_string(), value.value().to_vec()));
        }
        Ok(results)
    }
    
    // ========== Deployment Operations ==========
    
    /// Store a deployment record
    pub fn set_deployment(&self, record: &DeploymentRecord) -> Result<()> {
        let json = serde_json::to_vec(record)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(DEPLOYMENTS_TABLE)?;
            table.insert(record.workload_id.as_str(), json.as_slice())?;
        }
        write_txn.commit()?;
        debug!("Stored deployment record for workload_id={}", record.workload_id);
        Ok(())
    }
    
    /// Get a deployment record by workload ID
    pub fn get_deployment(&self, workload_id: &str) -> Result<Option<DeploymentRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEPLOYMENTS_TABLE)?;
        
        match table.get(workload_id)? {
            Some(value) => {
                let record: DeploymentRecord = serde_json::from_slice(value.value())
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
    
    /// Remove a deployment record
    pub fn remove_deployment(&self, workload_id: &str) -> Result<Option<DeploymentRecord>> {
        let write_txn = self.db.begin_write()?;
        let result = {
            let mut table = write_txn.open_table(DEPLOYMENTS_TABLE)?;
            match table.remove(workload_id)? {
                Some(value) => {
                    let record: DeploymentRecord = serde_json::from_slice(value.value())
                        .map_err(|e| StorageError::Serialization(e.to_string()))?;
                    Some(record)
                }
                None => None,
            }
        };
        write_txn.commit()?;
        Ok(result)
    }
    
    /// List all deployment records
    pub fn list_deployments(&self) -> Result<Vec<DeploymentRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEPLOYMENTS_TABLE)?;
        
        let mut results = Vec::new();
        for entry in table.iter()? {
            let (_key, value) = entry?;
            match serde_json::from_slice::<DeploymentRecord>(value.value()) {
                Ok(record) => results.push(record),
                Err(e) => {
                    warn!("Failed to deserialize deployment record: {}", e);
                }
            }
        }
        Ok(results)
    }
    
    /// Get deployments by manifest ID
    pub fn get_deployments_by_manifest(&self, manifest_id: &str) -> Result<Vec<DeploymentRecord>> {
        let all = self.list_deployments()?;
        Ok(all.into_iter().filter(|r| r.manifest_id == manifest_id).collect())
    }
    
    /// Update deployment status
    pub fn update_deployment_status(&self, workload_id: &str, status: &str) -> Result<bool> {
        if let Some(mut record) = self.get_deployment(workload_id)? {
            record.status = status.to_string();
            record.last_updated_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.set_deployment(&record)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Initialize the global state store
pub fn init_global_state_store(config: StateStoreConfig) -> Result<()> {
    let store = StateStore::open(&config)?;
    GLOBAL_STATE_STORE
        .set(Arc::new(store))
        .map_err(|_| StorageError::Serialization("Global state store already initialized".into()))?;
    Ok(())
}

/// Get the global state store instance
pub fn get_global_state_store() -> Option<Arc<StateStore>> {
    GLOBAL_STATE_STORE.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    fn create_test_store() -> StateStore {
        StateStore::open_ephemeral().expect("Failed to create test store")
    }
    
    #[test]
    fn test_manifest_owner_crud() {
        let store = create_test_store();
        let manifest_id = "test-manifest-123";
        let owner_pubkey = vec![1, 2, 3, 4, 5];
        
        // Initially empty
        assert!(store.get_manifest_owner(manifest_id).unwrap().is_none());
        
        // Set owner
        store.set_manifest_owner(manifest_id, &owner_pubkey).unwrap();
        
        // Get owner
        let retrieved = store.get_manifest_owner(manifest_id).unwrap();
        assert_eq!(retrieved, Some(owner_pubkey.clone()));
        
        // Update owner
        let new_pubkey = vec![6, 7, 8, 9];
        store.set_manifest_owner(manifest_id, &new_pubkey).unwrap();
        let retrieved = store.get_manifest_owner(manifest_id).unwrap();
        assert_eq!(retrieved, Some(new_pubkey));
        
        // Remove owner
        let removed = store.remove_manifest_owner(manifest_id).unwrap();
        assert!(removed.is_some());
        assert!(store.get_manifest_owner(manifest_id).unwrap().is_none());
    }
    
    #[test]
    fn test_deployment_crud() {
        let store = create_test_store();
        let record = DeploymentRecord::new(
            "workload-123".to_string(),
            "manifest-456".to_string(),
            "Running".to_string(),
            "podman".to_string(),
            vec![1, 2, 3],
        );
        
        // Initially empty
        assert!(store.get_deployment("workload-123").unwrap().is_none());
        
        // Set deployment
        store.set_deployment(&record).unwrap();
        
        // Get deployment
        let retrieved = store.get_deployment("workload-123").unwrap().unwrap();
        assert_eq!(retrieved.workload_id, "workload-123");
        assert_eq!(retrieved.manifest_id, "manifest-456");
        assert_eq!(retrieved.status, "Running");
        
        // Update status
        store.update_deployment_status("workload-123", "Stopped").unwrap();
        let updated = store.get_deployment("workload-123").unwrap().unwrap();
        assert_eq!(updated.status, "Stopped");
        
        // Remove deployment
        let removed = store.remove_deployment("workload-123").unwrap();
        assert!(removed.is_some());
        assert!(store.get_deployment("workload-123").unwrap().is_none());
    }
    
    #[test]
    fn test_list_operations() {
        let store = create_test_store();
        
        // Add multiple owners
        store.set_manifest_owner("m1", &[1, 2]).unwrap();
        store.set_manifest_owner("m2", &[3, 4]).unwrap();
        
        let owners = store.list_manifest_owners().unwrap();
        assert_eq!(owners.len(), 2);
        
        // Add multiple deployments
        store.set_deployment(&DeploymentRecord::new(
            "w1".to_string(),
            "m1".to_string(),
            "Running".to_string(),
            "podman".to_string(),
            vec![1],
        )).unwrap();
        store.set_deployment(&DeploymentRecord::new(
            "w2".to_string(),
            "m1".to_string(),
            "Running".to_string(),
            "podman".to_string(),
            vec![1],
        )).unwrap();
        
        let deployments = store.list_deployments().unwrap();
        assert_eq!(deployments.len(), 2);
        
        let by_manifest = store.get_deployments_by_manifest("m1").unwrap();
        assert_eq!(by_manifest.len(), 2);
    }
}
