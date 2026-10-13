use super::*;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    owner: String,
    catalogs: Vec<ServiceCatalog>,
    addresses: Vec<(ServiceKey, Ipv4Addr)>,
    next: u32,
}

impl ServiceCache {
    /// Only authenticated owner control may select a new catalog/generation.
    /// Ordinary peer synchronization still rejects such replacement.
    pub fn seed(&mut self, record: ReplicaServiceRecord, now: u64) -> Result<()> {
        record.verify(&self.owner, now)?;
        self.replace_catalog(record.catalog.clone())?;
        let slot = (record.catalog.manifest_id.clone(), record.replica_index);
        if self.records.get(&slot).is_some_and(|old| {
            old.credential.delegation != record.credential.delegation
                || old.endpoint.endpoint_id != record.endpoint.endpoint_id
        }) {
            self.records.remove(&slot);
        }
        self.admit(record, now)?;
        Ok(())
    }
    pub fn replace_catalog(&mut self, catalog: ServiceCatalog) -> Result<()> {
        catalog.verify(&self.owner)?;
        let additions = catalog
            .services
            .iter()
            .filter(|service| {
                !self.addresses.contains_key(&ServiceKey {
                    manifest_id: catalog.manifest_id.clone(),
                    name: service.name.clone(),
                })
            })
            .count();
        ensure!(
            self.addresses.len() + additions <= MAX_CACHED_SERVICES,
            "service cache capacity reached"
        );
        if self
            .catalogs
            .get(&catalog.manifest_id)
            .is_some_and(|old| old != &catalog)
        {
            self.records
                .retain(|(manifest, _), _| manifest != &catalog.manifest_id);
            self.names
                .retain(|_, key| key.manifest_id != catalog.manifest_id);
            self.catalogs.remove(&catalog.manifest_id);
        }
        self.authorize_catalog(catalog)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let snapshot = Snapshot {
            owner: self.owner.clone(),
            catalogs: self.catalogs.values().cloned().collect(),
            addresses: self
                .addresses
                .iter()
                .map(|(key, address)| (key.clone(), *address))
                .collect(),
            next: self.next_address,
        };
        let bytes = serde_json::to_vec(&snapshot)?;
        ensure!(bytes.len() <= 512 * 1024, "DNS state exceeds limit");
        atomic_state(path, &bytes)
    }
    pub fn restore(owner: String, path: &Path) -> Result<Self> {
        if !path.exists() {
            return Self::new(owner);
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(512 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 512 * 1024, "DNS state exceeds limit");
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        ensure!(
            snapshot.owner == owner
                && snapshot.addresses.len() <= MAX_CACHED_SERVICES
                && snapshot.catalogs.len() <= MAX_CACHED_SERVICES,
            "DNS state scope/count mismatch"
        );
        let mut cache = Self::new(owner)?;
        let mut used = std::collections::HashSet::new();
        for (key, address) in snapshot.addresses {
            ensure!(
                address.octets()[..3] == [198, 18, 0]
                    && address.octets()[3] > 0
                    && address.octets()[3] <= MAX_CACHED_SERVICES as u8
                    && used.insert(address),
                "invalid persisted VIP"
            );
            ensure!(
                cache.addresses.insert(key, address).is_none(),
                "duplicate persisted VIP key"
            );
        }
        cache.next_address = snapshot.next;
        ensure!(
            cache.next_address > u32::from(used.iter().map(|ip| ip.octets()[3]).max().unwrap_or(0))
                && cache.next_address <= MAX_CACHED_SERVICES as u32 + 1,
            "invalid persisted allocator"
        );
        for catalog in snapshot.catalogs {
            cache.authorize_catalog(catalog)?;
        }
        Ok(cache)
    }
}

pub(crate) fn atomic_state(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".state-{}", crypto::generate_secure_nonce()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
