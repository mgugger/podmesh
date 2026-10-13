//! Endpoint-bound identities for in-process workload-plane tests. Production
//! enrollment uses the selected agent issuer and the connector's fresh proof.
use protocol::workload_identity::{
    EnrollmentDelegation, EnrollmentScope, MAX_IDENTITY_LIFETIME_SECS, WorkloadIdentity,
};

pub fn workload_identity(
    owner_private: &[u8],
    owner: &str,
    name: &str,
    node: &iroh_support::NodeIdentity,
    replica: u32,
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    workload_identity_at(
        owner_private,
        owner,
        name,
        node,
        replica,
        now,
        3600,
        "test-generation",
    )
}

#[allow(clippy::too_many_arguments)]
pub fn workload_identity_at(
    owner_private: &[u8],
    owner: &str,
    name: &str,
    node: &iroh_support::NodeIdentity,
    replica: u32,
    issued: u64,
    lifetime: u64,
    generation: &str,
) -> String {
    let public = crypto::b64_decode(owner).unwrap();
    let parent = EnrollmentDelegation::mint(
        owner_private,
        EnrollmentScope {
            tenant_owner: owner.into(),
            manifest_id: protocol::route_id(&public, name),
            workload_id: protocol::workload_id(&public, name, replica),
            revision_id: protocol::revision_id(name.as_bytes()),
            issuer_public_key_b64: owner.into(),
            token_id: generation.into(),
            issued_at_secs: issued,
            expires_at_secs: None,
            max_child_lifetime_secs: MAX_IDENTITY_LIFETIME_SECS,
        },
        issued,
    )
    .unwrap();
    WorkloadIdentity::mint(
        owner_private,
        parent,
        node.endpoint_id().to_string(),
        crypto::b64_encode(node.signing_public()),
        issued,
        lifetime,
    )
    .unwrap()
    .to_b64()
    .unwrap()
}
