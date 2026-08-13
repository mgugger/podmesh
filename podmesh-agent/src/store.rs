use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

const WORKLOADS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("workloads");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredWorkload {
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

impl AgentStore {
    pub fn open(path: &Path, kem_public: Vec<u8>, kem_private: Vec<u8>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict(parent, DIRECTORY_MODE)?;
        }
        let db = Database::create(path)?;
        // The records are ciphertext, but the file still discloses how many
        // workloads this agent runs and how they churn. Keep it owner-only.
        restrict(path, FILE_MODE)?;
        let write = db.begin_write()?;
        write.open_table(WORKLOADS_TABLE)?;
        write.commit()?;
        Ok(Self {
            db,
            kem_public,
            kem_private,
        })
    }

    pub fn load_all(&self) -> Result<Vec<StoredWorkload>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(WORKLOADS_TABLE)?;
        let mut workloads = Vec::new();
        for entry in table.iter()? {
            let (key, blob) = entry?;
            let plaintext =
                crypto::decrypt_payload_from_recipient_blob(blob.value(), &self.kem_private)
                    .with_context(|| format!("decrypt local workload state for {}", key.value()))?;
            let workload: StoredWorkload = postcard::from_bytes(&plaintext)
                .with_context(|| format!("decode local workload state for {}", key.value()))?;
            anyhow::ensure!(
                workload.grant.workload_id == key.value(),
                "stored workload key does not match signed grant"
            );
            workloads.push(workload);
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
