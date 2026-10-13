use iroh_support::NodeIdentity;
use protocol::{
    service_discovery::{
        ReplicaServiceRecord, SERVICE_RECORD_VERSION, ServiceCatalog, ServiceDefinition,
        ServicePort,
    },
    workload_identity::{EnrollmentDelegation, EnrollmentScope, WorkloadIdentity},
};

pub struct Authority {
    pub owner: String,
    owner_private: Vec<u8>,
    issuer: String,
    issuer_private: Vec<u8>,
}
impl Authority {
    pub fn new() -> Self {
        let (public, private) = crypto::generate_signing_keypair();
        let (issuer, issuer_private) = crypto::generate_signing_keypair();
        Self {
            owner: crypto::b64_encode(&public),
            owner_private: private,
            issuer: crypto::b64_encode(&issuer),
            issuer_private,
        }
    }
    pub fn catalog(&self, target: u16) -> ServiceCatalog {
        self.catalog_named(target, "api")
    }
    pub fn catalog_named(&self, target: u16, name: &str) -> ServiceCatalog {
        ServiceCatalog {
            version: SERVICE_RECORD_VERSION,
            owner: self.owner.clone(),
            workload_name: "demo".into(),
            manifest_id: protocol::route_id(&crypto::b64_decode(&self.owner).unwrap(), "demo"),
            revision_id: protocol::revision_id(b"public service definition"),
            services: vec![ServiceDefinition {
                name: name.into(),
                ports: vec![ServicePort {
                    port: 80,
                    target_port: target,
                }],
            }],
            signature: String::new(),
        }
        .sign(&self.owner_private)
        .unwrap()
    }
    pub fn record(
        &self,
        catalog: &ServiceCatalog,
        node: &NodeIdentity,
        addresses: Vec<String>,
        index: u32,
        now: u64,
        lifetime: u64,
    ) -> ReplicaServiceRecord {
        let scope = EnrollmentScope {
            tenant_owner: self.owner.clone(),
            manifest_id: catalog.manifest_id.clone(),
            workload_id: protocol::workload_id(
                &crypto::b64_decode(&self.owner).unwrap(),
                "demo",
                index,
            ),
            revision_id: catalog.revision_id.clone(),
            issuer_public_key_b64: self.issuer.clone(),
            token_id: format!("generation-{index}"),
            issued_at_secs: now,
            expires_at_secs: None,
            max_child_lifetime_secs: 3600,
        };
        let parent = EnrollmentDelegation::mint(&self.owner_private, scope, now).unwrap();
        let credential = WorkloadIdentity::mint(
            &self.issuer_private,
            parent,
            node.endpoint_id().to_string(),
            crypto::b64_encode(node.signing_public()),
            now,
            600,
        )
        .unwrap();
        let endpoint = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: node.endpoint_id().as_bytes().to_vec(),
            relay_url: None,
            direct_addresses: addresses,
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 600,
            signature: String::new(),
        }
        .sign(node.signing_public(), node.signing_private(), now)
        .unwrap();
        ReplicaServiceRecord {
            version: SERVICE_RECORD_VERSION,
            catalog: catalog.clone(),
            credential,
            endpoint,
            replica_index: index,
            sequence: 1,
            issued_at_secs: now,
            expires_at_secs: now + lifetime,
            signature: String::new(),
        }
        .sign(node.signing_private(), now)
        .unwrap()
    }
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
