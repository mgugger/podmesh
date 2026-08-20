use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const WORKLOADS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("workloads");
pub const STORED_WORKLOAD_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StoredWorkloadPhase {
    Active,
    Updating {
        previous_grant: Box<protocol::DeploymentGrant>,
        previous_runtime_id: String,
        request: Box<protocol::UpdateRequest>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredWorkload {
    pub version: u16,
    pub phase: StoredWorkloadPhase,
    pub grant: protocol::DeploymentGrant,
    pub runtime_id: String,
    pub deleting: bool,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    /// Copied out of the encrypted execution spec at deploy time so the agent
    /// can answer an owner's list request without decrypting every record.
    pub workload_name: String,
    pub replica_index: u32,
    pub replica_count: u32,
}

pub struct AgentStore {
    db: Database,
    kem_public: Vec<u8>,
    kem_private: Vec<u8>,
}

pub(crate) trait WorkloadStore: Send + Sync {
    fn load_all(&self) -> Result<Vec<(String, Result<StoredWorkload>)>>;
    fn save(&self, workload: &StoredWorkload) -> Result<()>;
    fn remove(&self, workload_id: &str) -> Result<()>;
}

/// Mode for the state file. It holds encrypted tenant records.
const FILE_MODE: u32 = 0o600;
/// Mode for the directory holding the state file.
const DIRECTORY_MODE: u32 = 0o700;

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut missing = Vec::<PathBuf>::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current
            .parent()
            .ok_or_else(|| anyhow::anyhow!("private state directory has no existing ancestor"))?;
    }
    for directory in missing.into_iter().rev() {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(DIRECTORY_MODE);
        builder
            .create(&directory)
            .with_context(|| format!("create private directory {}", directory.display()))?;
    }
    restrict(path, DIRECTORY_MODE)
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create state directory {}", path.display()))
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if path.exists() {
        return restrict(path, FILE_MODE);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
        .with_context(|| format!("create private state file {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_file(_path: &Path) -> Result<()> {
    Ok(())
}

impl AgentStore {
    pub fn open(path: &Path, kem_public: Vec<u8>, kem_private: Vec<u8>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            create_private_dir_all(parent)?;
        }

        create_private_file(path)?;
        let db = Database::create(path)?;
        let write = db.begin_write()?;
        write.open_table(WORKLOADS_TABLE)?;
        write.commit()?;
        Ok(Self {
            db,
            kem_public,
            kem_private,
        })
    }

    pub fn load_all(&self) -> Result<Vec<(String, Result<StoredWorkload>)>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(WORKLOADS_TABLE)?;
        let mut workloads = Vec::new();
        for entry in table.iter()? {
            let (key, blob) = entry?;
            let workload_id = key.value().to_string();
            let result = (|| {
                let plaintext =
                    crypto::decrypt_payload_from_recipient_blob(blob.value(), &self.kem_private)
                        .with_context(|| {
                            format!("decrypt local workload state for {workload_id}")
                        })?;
                let workload: StoredWorkload = postcard::from_bytes(&plaintext)
                    .with_context(|| format!("decode local workload state for {workload_id}"))?;
                anyhow::ensure!(
                    workload.version == STORED_WORKLOAD_VERSION,
                    "unsupported stored workload version"
                );
                anyhow::ensure!(
                    workload.grant.workload_id == workload_id,
                    "stored workload key does not match signed grant"
                );
                Ok(workload)
            })();
            workloads.push((workload_id, result));
        }
        Ok(workloads)
    }

    pub fn save(&self, workload: &StoredWorkload) -> Result<()> {
        let plaintext = postcard::to_allocvec(workload)?;
        let blob = crypto::encrypt_payload_for_recipient(&self.kem_public, &plaintext)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(WORKLOADS_TABLE)?;
            table.insert(workload.grant.workload_id.as_str(), blob.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn remove(&self, workload_id: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(WORKLOADS_TABLE)?;
            table.remove(workload_id)?;
        }
        write.commit()?;
        Ok(())
    }
}

impl WorkloadStore for AgentStore {
    fn load_all(&self) -> Result<Vec<(String, Result<StoredWorkload>)>> {
        Self::load_all(self)
    }

    fn save(&self, workload: &StoredWorkload) -> Result<()> {
        Self::save(self, workload)
    }

    fn remove(&self, workload_id: &str) -> Result<()> {
        Self::remove(self, workload_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_workload_is_returned_as_a_per_key_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.redb");
        let store = AgentStore::open(&path, vec![7; 32], vec![8; 32]).unwrap();
        let write = store.db.begin_write().unwrap();
        {
            let mut table = write.open_table(WORKLOADS_TABLE).unwrap();
            table.insert("corrupt", &[1_u8, 2, 3][..]).unwrap();
        }
        write.commit().unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, "corrupt");
        assert!(loaded[0].1.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn new_store_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private").join("state.redb");
        AgentStore::open(&path, vec![7; 32], vec![8; 32]).unwrap();
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            DIRECTORY_MODE
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            FILE_MODE
        );
    }
}
