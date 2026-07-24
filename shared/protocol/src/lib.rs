pub mod node_cert;
pub use node_cert::{Endorsement, NodeCert, NodeRole};

pub mod agent;
pub use agent::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, AgentAdvertisement, DeploymentGrant,
    DeploymentReceipt, EncryptedWorkloadCapsule, ExecutionSpec, Reservation, WorkloadCommand,
    WorkloadCommandResponse, WorkloadOperation, revision_id, workload_id,
};

pub mod egress;
pub mod libp2p_constants;
pub mod machine;
pub mod manifest_policy;
pub mod manifest_resources;
pub use manifest_resources::{ManifestResources, validate_and_measure_manifest};
pub mod manifest_yaml;
pub mod podmesh_annotations;
pub mod sidecar_metadata;
pub use podmesh_annotations::PodmeshAnnotations;
pub mod sidecar_registration;
pub mod tenant_dht;
pub use sidecar_registration::{SidecarRegistration, SidecarRegistrationAck, SidecarRoute};
pub use tenant_dht::{
    compute_tenant_proxy_dht_key, compute_tenant_proxy_dht_key_from_bytes,
    compute_tenant_proxy_dht_key_hex,
};

#[cfg(test)]
mod tests {
    use super::machine::{
        SignedEnvelopeParams, build_envelope_canonical, build_envelope_signed, root_as_envelope,
    };

    #[test]
    fn canonical_and_signed_envelope_share_payload() {
        let payload = b"demo";
        let canonical = build_envelope_canonical(payload, "demo", "nonce", 1, "ed25519", None);
        let signed = build_envelope_signed(SignedEnvelopeParams {
            payload,
            payload_type: "demo",
            nonce: "nonce",
            timestamp: 1,
            algorithm: "ed25519",
            signature_prefix: "ed25519",
            signature_b64: "c2ln",
            public_key_b64: "cHVi",
            peer_id: None,
            kem_public_key_b64: None,
        });
        let canonical_env = root_as_envelope(&canonical).expect("canonical envelope");
        let signed_env = root_as_envelope(&signed).expect("signed envelope");
        assert_eq!(canonical_env.payload(), Some(&payload[..]));
        assert_eq!(canonical_env.payload(), signed_env.payload());
    }
}
