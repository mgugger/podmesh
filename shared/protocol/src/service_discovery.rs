//! Public owner-authorized service definitions and expiring replica records.
//! Neither an endpoint's self-signature nor tenant membership alone authorizes
//! arbitrary service names or application ports.
use crate::{EndpointRecord, workload_identity::WorkloadIdentity};
mod manifest;
pub use manifest::{extract_services, workload_services};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const SERVICE_RECORD_VERSION: u16 = 1;
pub const MAX_SERVICE_RECORD_BYTES: usize = 20 * 1024;
pub const MAX_CATALOG_SERVICES: usize = 32;
pub const MAX_SERVICE_PORTS: usize = 16;
pub const MAX_SERVICE_LEASE_SECS: u64 = 120;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServicePort {
    pub port: u16,
    pub target_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceDefinition {
    pub name: String,
    pub ports: Vec<ServicePort>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceCatalog {
    pub version: u16,
    pub owner: String,
    pub workload_name: String,
    pub manifest_id: String,
    pub revision_id: String,
    pub services: Vec<ServiceDefinition>,
    pub signature: String,
}

impl ServiceCatalog {
    fn canonical(&self) -> Result<Vec<u8>> {
        let mut value = self.clone();
        value.signature.clear();
        Ok(postcard::to_allocvec(&value)?)
    }
    pub fn sign(mut self, private: &[u8]) -> Result<Self> {
        self.validate()?;
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            private,
            crypto::SignatureDomain::ServiceCatalog,
            &self.canonical()?,
        )?);
        self.verify(&self.owner)?;
        Ok(self)
    }
    pub fn verify(&self, owner: &str) -> Result<()> {
        self.validate()?;
        ensure!(
            self.owner == owner,
            "service catalog belongs to another tenant"
        );
        crypto::verify_domain(
            &crypto::b64_decode(owner)?,
            crypto::SignatureDomain::ServiceCatalog,
            &self.canonical()?,
            &crypto::b64_decode(&self.signature)?,
        )
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == SERVICE_RECORD_VERSION,
            "unsupported service catalog version"
        );
        ensure!(
            !self.workload_name.is_empty() && self.workload_name.len() <= 253,
            "invalid catalog workload name"
        );
        let owner = crypto::b64_decode(&self.owner)?;
        ensure!(
            owner.len() == 32 && self.manifest_id == crate::route_id(&owner, &self.workload_name),
            "invalid catalog routing identity"
        );
        ensure!(
            self.revision_id.len() == 64
                && self
                    .revision_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid catalog revision"
        );
        ensure!(
            !self.services.is_empty() && self.services.len() <= MAX_CATALOG_SERVICES,
            "invalid catalog service count"
        );
        let mut names = std::collections::HashSet::new();
        for service in &self.services {
            validate_service_label(&service.name)?;
            ensure!(names.insert(&service.name), "duplicate service name");
            ensure!(
                !service.ports.is_empty() && service.ports.len() <= MAX_SERVICE_PORTS,
                "invalid service port count"
            );
            let mut ports = std::collections::HashSet::new();
            for port in &service.ports {
                ensure!(
                    port.port != 0 && port.target_port != 0 && ports.insert(port.port),
                    "invalid or duplicate service port"
                );
            }
        }
        ensure!(
            self.canonical()?.len() <= 4096,
            "service catalog exceeds limit"
        );
        Ok(())
    }
}

pub fn validate_service_label(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 63
            && !value.starts_with('-')
            && !value.ends_with('-')
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid DNS service label"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplicaServiceRecord {
    pub version: u16,
    pub catalog: ServiceCatalog,
    pub credential: WorkloadIdentity,
    pub endpoint: EndpointRecord,
    pub replica_index: u32,
    pub sequence: u64,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signature: String,
}

impl ReplicaServiceRecord {
    fn canonical(&self) -> Result<Vec<u8>> {
        let mut value = self.clone();
        value.signature.clear();
        Ok(postcard::to_allocvec(&value)?)
    }
    pub fn sign(mut self, private: &[u8], now: u64) -> Result<Self> {
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            private,
            crypto::SignatureDomain::ReplicaServiceRecord,
            &self.canonical()?,
        )?);
        self.verify(&self.catalog.owner, now)?;
        self.to_bytes()?;
        Ok(self)
    }
    pub fn verify(&self, owner: &str, now: u64) -> Result<()> {
        self.verify_structure(owner)?;
        self.endpoint.verify(now)?;
        self.credential.verify_session(
            owner,
            &self.catalog.manifest_id,
            &endpoint_hex(&self.endpoint),
            &crypto::b64_decode(&self.endpoint.signing_pubkey)?,
            now,
        )?;
        ensure!(
            self.issued_at_secs <= now.saturating_add(60) && self.expires_at_secs > now,
            "replica lease expired or invalid"
        );
        Ok(())
    }
    /// Previously verified records may bootstrap an address, never authorize a
    /// service. The live response still proves fresh, bound authority.
    pub fn verify_structure(&self, owner: &str) -> Result<()> {
        ensure!(
            self.version == SERVICE_RECORD_VERSION,
            "unsupported replica record version"
        );
        self.catalog.verify(owner)?;
        self.endpoint.verify_structure()?;
        for address in &self.endpoint.direct_addresses {
            ensure!(
                address.parse::<std::net::SocketAddr>()?.port() != 53,
                "Iroh DNS-port hints are not supported"
            );
        }
        self.credential.verify(None)?;
        let scope = &self.credential.delegation.scope;
        ensure!(
            scope.tenant_owner == owner
                && scope.manifest_id == self.catalog.manifest_id
                && self.credential.claims.endpoint_id == endpoint_hex(&self.endpoint)
                && self.credential.claims.signing_public_key_b64 == self.endpoint.signing_pubkey,
            "replica key binding mismatch"
        );
        ensure!(
            scope.revision_id == self.catalog.revision_id
                && scope.workload_id
                    == crate::workload_id(
                        &crypto::b64_decode(owner)?,
                        &self.catalog.workload_name,
                        self.replica_index
                    ),
            "replica record exceeds credential scope"
        );
        ensure!(
            self.replica_index < crate::MAX_WORKLOAD_REPLICAS && self.sequence > 0,
            "invalid replica index or sequence"
        );
        ensure!(
            self.expires_at_secs > self.issued_at_secs
                && self.expires_at_secs - self.issued_at_secs <= MAX_SERVICE_LEASE_SECS,
            "replica lease expired or invalid"
        );
        ensure!(
            self.expires_at_secs <= self.credential.claims.expires_at_secs
                && self.expires_at_secs <= self.endpoint.expires_at_secs,
            "replica lease outlives identity/address authority"
        );
        crypto::verify_domain(
            &crypto::b64_decode(&self.endpoint.signing_pubkey)?,
            crypto::SignatureDomain::ReplicaServiceRecord,
            &self.canonical()?,
            &crypto::b64_decode(&self.signature)?,
        )
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_SERVICE_RECORD_BYTES,
            "replica record exceeds limit"
        );
        Ok(bytes)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_SERVICE_RECORD_BYTES,
            "invalid replica record size"
        );
        serde_json::from_slice(bytes).context("decode replica service record")
    }
}

pub fn endpoint_hex(record: &EndpointRecord) -> String {
    record
        .endpoint_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
