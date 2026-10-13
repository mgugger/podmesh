//! Bounded soft-state service view. Admission verifies owner catalogs, bound
//! credentials and signed leases; DNS never treats a public address as authority.
use anyhow::{Context, Result, ensure};
use protocol::service_discovery::{ReplicaServiceRecord, ServiceCatalog, validate_service_label};
mod state;
pub(crate) use state::atomic_state;

use std::{
    collections::{BTreeMap, HashMap},
    net::Ipv4Addr,
};

pub const INTERNAL_SUFFIX: &str = "svc.podmesh.internal.";
pub const MAX_CACHED_REPLICAS: usize = 512;
pub const MAX_CACHED_SERVICES: usize = 128;

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ServiceKey {
    pub manifest_id: String,
    pub name: String,
}

/// Encode the full 256-bit route identity, never its invalid 64-octet hex label.
pub fn route_label(route: &str) -> Result<String> {
    ensure!(
        route.len() == 64
            && route
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid route digest"
    );
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&route[index * 2..index * 2 + 2], 16)
            .context("invalid route digest")?;
    }
    Ok(data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase())
}

impl ServiceKey {
    pub fn fqdn(&self) -> Result<String> {
        validate_service_label(&self.name)?;
        Ok(format!(
            "{}.{}.{}",
            self.name,
            route_label(&self.manifest_id)?,
            INTERNAL_SUFFIX
        ))
    }
}

pub fn is_internal_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let suffix = INTERNAL_SUFFIX.trim_end_matches('.');
    name == suffix || name.ends_with(&format!(".{suffix}"))
}

#[derive(Clone, Debug)]
pub struct ServiceAnswer {
    pub key: ServiceKey,
    pub address: Ipv4Addr,
    pub ttl: u32,
}

pub struct ServiceCache {
    owner: String,
    catalogs: HashMap<String, ServiceCatalog>,
    records: BTreeMap<(String, u32), ReplicaServiceRecord>,
    addresses: BTreeMap<ServiceKey, Ipv4Addr>,
    names: HashMap<String, ServiceKey>,
    next_address: u32,
    address_base: u32,
    cursors: HashMap<ServiceKey, usize>,
}

impl ServiceCache {
    pub fn new(owner: String) -> Result<Self> {
        ensure!(
            crypto::b64_decode(&owner)?.len() == 32,
            "invalid cache owner"
        );
        Ok(Self {
            owner,
            catalogs: HashMap::new(),
            records: BTreeMap::new(),
            addresses: BTreeMap::new(),
            names: HashMap::new(),
            next_address: 1,
            address_base: u32::from(Ipv4Addr::new(198, 18, 0, 0)),
            cursors: HashMap::new(),
        })
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Explicit owner-authorized catalog admission. The initial API deliberately
    /// refuses revision changes: hot catalog/generation replacement needs an
    /// explicit owner-intent transition, not wall-clock last-writer-wins.
    pub fn authorize_catalog(&mut self, catalog: ServiceCatalog) -> Result<()> {
        catalog.verify(&self.owner)?;
        if let Some(old) = self.catalogs.get(&catalog.manifest_id) {
            ensure!(
                old == &catalog,
                "catalog revision/conflict requires explicit replacement"
            );
            return Ok(());
        }
        let new_services = catalog
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
            self.addresses.len() + new_services <= MAX_CACHED_SERVICES,
            "service cache capacity reached"
        );
        let mut additions = Vec::new();
        let mut allocated = 0;
        for service in &catalog.services {
            let key = ServiceKey {
                manifest_id: catalog.manifest_id.clone(),
                name: service.name.clone(),
            };
            let name = key.fqdn()?;
            let old = self.addresses.get(&key).copied();
            let address = old.unwrap_or_else(|| {
                Ipv4Addr::from(self.address_base + self.next_address + allocated)
            });
            if old.is_none() {
                allocated += 1;
            }
            additions.push((key, name, address));
        }
        self.next_address += allocated;
        for (key, name, address) in additions {
            self.names.insert(name, key.clone());
            self.addresses.insert(key, address);
        }
        // A workload-level alias is unambiguous only for a one-Service catalog.
        if catalog.services.len() == 1 {
            let alias = format!("{}.{}", route_label(&catalog.manifest_id)?, INTERNAL_SUFFIX);
            self.names.insert(
                alias,
                ServiceKey {
                    manifest_id: catalog.manifest_id.clone(),
                    name: catalog.services[0].name.clone(),
                },
            );
        }
        self.catalogs.insert(catalog.manifest_id.clone(), catalog);
        self.rebuild_names()?;
        Ok(())
    }

    pub fn admit(&mut self, record: ReplicaServiceRecord, now: u64) -> Result<bool> {
        self.admit_inner(record, now, false)
    }
    /// Anti-entropy may carry older, still authenticated snapshots. Validate
    /// their authority/scope/keys but do not abort a batch or regress state.
    pub fn merge(&mut self, record: ReplicaServiceRecord, now: u64) -> Result<bool> {
        self.admit_inner(record, now, true)
    }
    fn admit_inner(
        &mut self,
        record: ReplicaServiceRecord,
        now: u64,
        ignore_older: bool,
    ) -> Result<bool> {
        record.to_bytes()?;
        record.verify(&self.owner, now)?;
        ensure!(
            self.catalogs.get(&record.catalog.manifest_id) == Some(&record.catalog),
            "replica catalog is not authorized locally"
        );
        let slot = (record.catalog.manifest_id.clone(), record.replica_index);
        if let Some(old) = self.records.get(&slot) {
            ensure!(
                old.credential.delegation == record.credential.delegation
                    && old.endpoint.endpoint_id == record.endpoint.endpoint_id
                    && old.endpoint.signing_pubkey == record.endpoint.signing_pubkey,
                "replica generation/key change requires explicit replacement"
            );
            if record.sequence < old.sequence {
                ensure!(ignore_older, "replica sequence regressed");
                return Ok(false);
            }
            if record.sequence == old.sequence {
                ensure!(old == &record, "replica sequence equivocation");
                return Ok(false);
            }
            ensure!(
                record.issued_at_secs >= old.issued_at_secs,
                "replica issue time regressed"
            );
        } else {
            ensure!(
                self.records.len() < MAX_CACHED_REPLICAS,
                "replica cache capacity reached"
            );
        }
        self.records.insert(slot, record);
        Ok(true)
    }

    pub fn lookup(&self, name: &str, now: u64) -> Option<ServiceAnswer> {
        let name = format!("{}.", name.trim_end_matches('.').to_ascii_lowercase());
        let key = self.names.get(&name)?;
        let records = self.eligible(key, now);
        let expires = records.iter().map(|record| record.expires_at_secs).min()?;
        Some(ServiceAnswer {
            key: key.clone(),
            address: *self.addresses.get(key)?,
            ttl: expires.saturating_sub(now).min(5) as u32,
        })
    }

    fn rebuild_names(&mut self) -> Result<()> {
        self.names.clear();
        let mut aliases: HashMap<String, Option<ServiceKey>> = HashMap::new();
        for catalog in self.catalogs.values() {
            for service in &catalog.services {
                let key = ServiceKey {
                    manifest_id: catalog.manifest_id.clone(),
                    name: service.name.clone(),
                };
                self.names.insert(key.fqdn()?, key.clone());
                for name in [
                    format!("{}.", service.name),
                    format!("{}.{}", service.name, INTERNAL_SUFFIX),
                ] {
                    aliases
                        .entry(name)
                        .and_modify(|value| {
                            if value.as_ref() != Some(&key) {
                                *value = None;
                            }
                        })
                        .or_insert(Some(key.clone()));
                }
                if catalog.services.len() == 1 {
                    self.names.insert(
                        format!("{}.{}", route_label(&catalog.manifest_id)?, INTERNAL_SUFFIX),
                        key.clone(),
                    );
                    if validate_service_label(&catalog.workload_name).is_ok() {
                        aliases
                            .entry(format!("{}.", catalog.workload_name))
                            .and_modify(|value| {
                                if value.as_ref() != Some(&key) {
                                    *value = None;
                                }
                            })
                            .or_insert(Some(key.clone()));
                    }
                }
            }
        }
        for (name, key) in aliases {
            if let Some(key) = key {
                self.names.insert(name, key);
            }
        }
        Ok(())
    }
    pub fn classifies_internal(&self, name: &str) -> bool {
        is_internal_name(name)
            || self.known_name(name)
            || self.catalogs.values().any(|catalog| {
                let label = name.trim_end_matches('.').to_ascii_lowercase();
                !label.contains('.')
                    && (catalog.workload_name == label
                        || catalog.services.iter().any(|service| service.name == label))
            })
    }
    pub fn known_name(&self, name: &str) -> bool {
        self.names.contains_key(&format!(
            "{}.",
            name.trim_end_matches('.').to_ascii_lowercase()
        ))
    }
    pub fn service_for_address(&self, address: Ipv4Addr) -> Option<ServiceKey> {
        self.addresses
            .iter()
            .find(|(_, value)| **value == address)
            .map(|(key, _)| key.clone())
    }
    pub fn target_port(&self, key: &ServiceKey, port: u16) -> Option<u16> {
        self.catalogs
            .get(&key.manifest_id)?
            .services
            .iter()
            .find(|service| service.name == key.name)?
            .ports
            .iter()
            .find(|mapping| mapping.port == port)
            .map(|mapping| mapping.target_port)
    }
    fn eligible(&self, key: &ServiceKey, now: u64) -> Vec<&ReplicaServiceRecord> {
        self.records
            .values()
            .filter(|record| {
                record.catalog.manifest_id == key.manifest_id
                    && record.expires_at_secs > now
                    && record.endpoint.expires_at_secs >= now
                    && record.credential.verify(Some(now)).is_ok()
            })
            .collect()
    }
    /// Rotation/retry is setup-only; callers never replay partially forwarded data.
    pub fn candidates(
        &mut self,
        key: &ServiceKey,
        now: u64,
        exclude: Option<&[u8]>,
    ) -> Vec<ReplicaServiceRecord> {
        let mut records = self
            .eligible(key, now)
            .into_iter()
            .filter(|record| exclude != Some(record.endpoint.endpoint_id.as_slice()))
            .cloned()
            .collect::<Vec<_>>();
        if !records.is_empty() {
            let cursor = self.cursors.entry(key.clone()).or_default();
            let index = *cursor % records.len();
            records.rotate_left(index);
            *cursor = cursor.wrapping_add(1);
        }
        records
    }
    pub fn snapshot(&self, now: u64) -> Vec<ReplicaServiceRecord> {
        self.records
            .values()
            .filter(|record| {
                record.expires_at_secs > now && record.credential.verify(Some(now)).is_ok()
            })
            .cloned()
            .collect()
    }
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}
