//! Sidecar route registration.
//!
//! A sidecar tells a proxy which paths it serves. The routing key is not
//! free-form: `manifest_id` is derived from the namespace owner key and the
//! workload name, and the proxy recomputes it. To register under some
//! `manifest_id` a sidecar must therefore name the exact owner key that hashes
//! to it — and the proxy only accepts registrations for owners it already holds
//! a live owner-signed grant for. One tenant consequently cannot claim another
//! tenant's routes.
//!
//! Every replica of a deployment registers the *same* routing key, because they
//! all serve the same workload. Each one identifies itself by `replica_index`,
//! so the proxy holds one backend per replica and can balance across them.
//! Keying backends on the replica index rather than on the sidecar's transport
//! identity matters: a restarted replica comes back with a fresh, ephemeral
//! endpoint id, and would otherwise appear as an additional backend beside the
//! dead one until the old entry timed out.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const SIDECAR_REGISTRATION_VERSION: u16 = 2;
/// Most routes one sidecar may announce.
pub const MAX_SIDECAR_ROUTES: usize = 64;
/// Longest hostname accepted for a route claim.
pub const MAX_ROUTE_HOST_LEN: usize = 253;
/// Longest path prefix accepted for a route.
pub const MAX_ROUTE_PATH_LEN: usize = 1024;

/// Derive the routing key for a workload.
///
/// Binding the key to the owner's public key is what makes route ownership
/// checkable: an attacker who wants a particular routing key must present the
/// owner key it was derived from, and the proxy will then demand a grant signed
/// by that owner's private key.
pub fn route_id(owner_pubkey: &[u8], workload_name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"podmesh/route/v1\0");
    hasher.update(owner_pubkey);
    hasher.update(b"\0");
    hasher.update(workload_name.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// A single route that a sidecar serves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarRoute {
    /// Virtual host this route answers for. Empty means "any host that resolves
    /// to this workload".
    pub host: String,
    pub path_prefix: String,
    pub port: u16,
}

impl SidecarRoute {
    pub fn host(&self) -> Option<&str> {
        if self.host.is_empty() {
            None
        } else {
            Some(self.host.as_str())
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.host.len() <= MAX_ROUTE_HOST_LEN,
            "route host exceeds {MAX_ROUTE_HOST_LEN} bytes"
        );
        ensure!(
            self.host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
            "route host contains characters outside the hostname alphabet"
        );
        ensure!(
            self.path_prefix.starts_with('/') && self.path_prefix.len() <= MAX_ROUTE_PATH_LEN,
            "route path prefix must be absolute and at most {MAX_ROUTE_PATH_LEN} bytes"
        );
        ensure!(self.port != 0, "route target port must be non-zero");
        Ok(())
    }
}

/// Sent by a sidecar to a proxy to register the routes it serves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarRegistration {
    pub version: u16,
    /// Base64 Ed25519 public key of the namespace owner.
    pub owner_pubkey: String,
    /// Workload name as written in the manifest.
    pub workload_name: String,
    /// `route_id(owner_pubkey, workload_name)`. Recomputed by the proxy.
    pub manifest_id: String,
    /// Which replica of the deployment this sidecar belongs to.
    pub replica_index: u32,
    /// How many replicas the owner asked for, used only for reporting.
    pub replica_count: u32,
    pub routes: Vec<SidecarRoute>,
    /// The sidecar's Iroh endpoint id, checked against the authenticated
    /// transport so a registration cannot name another peer's endpoint.
    pub sidecar_peer_id: String,
}

impl SidecarRegistration {
    pub fn new(
        owner_pubkey: &[u8],
        workload_name: &str,
        replica_index: u32,
        replica_count: u32,
        routes: Vec<SidecarRoute>,
        sidecar_peer_id: &str,
    ) -> Self {
        Self {
            version: SIDECAR_REGISTRATION_VERSION,
            owner_pubkey: crypto::b64_encode(owner_pubkey),
            workload_name: workload_name.to_string(),
            manifest_id: route_id(owner_pubkey, workload_name),
            replica_index,
            replica_count,
            routes,
            sidecar_peer_id: sidecar_peer_id.to_string(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(postcard::to_allocvec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value: Self = postcard::from_bytes(bytes)?;
        value.validate()?;
        Ok(value)
    }

    /// Structural checks that do not need proxy state.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == SIDECAR_REGISTRATION_VERSION,
            "unsupported sidecar registration version {}",
            self.version
        );
        let owner = crypto::b64_decode(&self.owner_pubkey)?;
        ensure!(
            owner.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
            "owner_pubkey must decode to {} bytes",
            crypto::ED25519_PUBLIC_KEY_SIZE
        );
        ensure!(
            !self.workload_name.is_empty() && self.workload_name.len() <= 253,
            "invalid workload name"
        );
        // The routing key is not the sidecar's to choose.
        ensure!(
            self.manifest_id == route_id(&owner, &self.workload_name),
            "manifest_id is not derived from this owner key and workload name"
        );
        ensure!(
            self.replica_count >= 1 && self.replica_count <= crate::MAX_WORKLOAD_REPLICAS,
            "replica count must be between 1 and {}",
            crate::MAX_WORKLOAD_REPLICAS
        );
        ensure!(
            self.replica_index < self.replica_count,
            "replica index is outside the deployment"
        );
        ensure!(
            !self.routes.is_empty() && self.routes.len() <= MAX_SIDECAR_ROUTES,
            "a registration must carry between 1 and {MAX_SIDECAR_ROUTES} routes"
        );
        for route in &self.routes {
            route.validate()?;
        }
        ensure!(
            !self.sidecar_peer_id.is_empty() && self.sidecar_peer_id.len() <= 128,
            "invalid sidecar peer id"
        );
        Ok(())
    }

    pub fn owner_key(&self) -> Result<Vec<u8>> {
        crypto::b64_decode(&self.owner_pubkey)
    }
}

/// Acknowledgement returned by the proxy after receiving a `SidecarRegistration`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarRegistrationAck {
    pub manifest_id: String,
    pub ok: bool,
    pub message: String,
}

impl SidecarRegistrationAck {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> Vec<SidecarRoute> {
        vec![SidecarRoute {
            host: "demo.mesh.local".into(),
            path_prefix: "/".into(),
            port: 8080,
        }]
    }

    fn registration(owner: &[u8], name: &str) -> SidecarRegistration {
        SidecarRegistration::new(owner, name, 0, 1, routes(), "peer")
    }

    #[test]
    fn round_trip_preserves_the_derived_routing_key() {
        let owner = [7u8; 32];
        let registration = registration(&owner, "demo");
        let decoded = SidecarRegistration::from_bytes(&registration.to_bytes().unwrap()).unwrap();
        assert_eq!(decoded.manifest_id, route_id(&owner, "demo"));
    }

    #[test]
    fn a_registration_cannot_claim_another_owners_routing_key() {
        let attacker = [1u8; 32];
        let victim = [2u8; 32];
        let mut registration = registration(&attacker, "demo");
        registration.manifest_id = route_id(&victim, "demo");
        let error = registration.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not derived from this owner key")
        );
    }

    #[test]
    fn routing_keys_differ_per_owner_and_per_workload() {
        assert_ne!(route_id(&[1u8; 32], "demo"), route_id(&[2u8; 32], "demo"));
        assert_ne!(route_id(&[1u8; 32], "a"), route_id(&[1u8; 32], "b"));
    }

    #[test]
    fn a_replica_index_outside_the_deployment_is_refused() {
        let owner = [7u8; 32];
        let mut registration = SidecarRegistration::new(&owner, "demo", 3, 3, routes(), "peer");
        assert!(registration.validate().is_err());
        registration.replica_count = 4;
        registration.validate().expect("index 3 of 4 is valid");
    }

    #[test]
    fn malformed_routes_are_refused() {
        let owner = [7u8; 32];
        let mut registration = registration(&owner, "demo");
        registration.routes[0].path_prefix = "no-leading-slash".into();
        assert!(registration.validate().is_err());

        let mut empty = SidecarRegistration::new(&owner, "demo", 0, 1, Vec::new(), "peer");
        empty.routes.clear();
        assert!(empty.validate().is_err());
    }
}
