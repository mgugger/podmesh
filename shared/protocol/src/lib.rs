pub mod agent;
pub use agent::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, DeploymentGrant, DeploymentReceipt,
    EncryptedWorkloadCapsule, ExecutionSpec, MAX_WORKLOAD_REPLICAS, Reservation, WorkloadCommand,
    WorkloadCommandResponse, WorkloadOperation, deployment_id, revision_id, workload_id,
};
pub mod agent_control;
pub use agent_control::{
    AGENT_CONTROL_ALPN, AGENT_CONTROL_PROTOCOL_VERSION, AgentControlError, AgentControlOperation,
    AgentControlRequest, AgentControlResponse, MAX_AGENT_CONTROL_FRAME_BYTES,
    MAX_AGENT_CONTROL_PAYLOAD_BYTES,
};

pub mod capacity;
pub use capacity::{
    CAPACITY_PROTOCOL_VERSION, CapacityOffer, CapacityQuery, MAX_CAPACITY_MESSAGE_BYTES,
};

pub mod egress;
pub mod endpoint_record;
pub use endpoint_record::{
    ENDPOINT_RECORD_VERSION, EndpointRecord, IROH_ENDPOINT_ID_BYTES, MAX_ENDPOINT_DIRECT_ADDRESSES,
    MAX_ENDPOINT_RECORD_BYTES,
};
pub mod http_proxy;
pub use http_proxy::{ProxyHttpRequest, ProxyHttpResponse};
pub mod iroh_frame;
pub use iroh_frame::{
    DEFAULT_IROH_FRAME_TIMEOUT, IrohFrame, MAX_IROH_FRAME_BYTES, OperationFrame, OperationKind,
    TenantSessionFrame, WorkloadPeerRole, read_iroh_frame, write_iroh_frame,
};
pub mod machine;
pub mod machine_relay;
pub use machine_relay::{
    MACHINE_RELAY_GRANT_VERSION, MAX_MACHINE_RELAY_AUTH_TOKEN_LEN, MAX_MACHINE_RELAY_GRANT_BYTES,
    MAX_MACHINE_RELAY_GRANT_LIFETIME_SECS, MachineRelayGrant, MachineRole,
};
pub mod manifest_policy;
pub mod manifest_resources;
pub use manifest_resources::{ManifestResources, validate_and_measure_manifest};
pub mod manifest_yaml;
pub mod placement;
pub mod podmesh_annotations;
pub mod sidecar_metadata;
pub use placement::{
    PLACEMENT_PROTOCOL_VERSION, PlacementError, PlacementRequest, PlacementResponse,
};
pub use podmesh_annotations::PodmeshAnnotations;
pub mod proxy_endpoint_discovery;
pub use proxy_endpoint_discovery::{ProxyDiscoveryRequest, ProxyEndpointDiscoveryResponse};
pub mod sidecar_registration;
pub use sidecar_registration::{SidecarRegistration, SidecarRegistrationAck, SidecarRoute};
pub mod scheduler_mesh;
pub use scheduler_mesh::{
    AGENT_CAPACITY_ALPN, AgentAttachmentAck, AgentAttachmentHello, CAPACITY_OFFER_ALPN,
    MAX_AGENT_ATTACHMENT_BYTES, SCHEDULER_MESH_PROTOCOL_VERSION, SCHEDULER_PLACEMENT_ALPN,
};
pub mod workload_authz;
pub use workload_authz::{
    MAX_BISCUIT_TOKEN_BYTES, WorkloadAuthorizationContext, WorkloadCapabilityClaims,
    WorkloadOperation as BiscuitWorkloadOperation, authorize_workload_biscuit,
    biscuit_keypair_from_ed25519, biscuit_public_key_from_ed25519, mint_workload_biscuit,
};
pub mod proxy_grant;
pub use proxy_grant::{
    MAX_PROXY_GRANT_B64_LEN, MAX_PROXY_GRANT_LIFETIME_SECS, ProxyGrantClaims, mint_proxy_grant,
    proxy_grant_from_b64, proxy_grant_to_b64, verify_proxy_grant,
};
pub mod workload_stream;
pub use workload_stream::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, MESH_DOMAIN_SUFFIX, WORKLOAD_ALPN, WorkloadStreamKind,
    read_workload_frame, write_workload_frame,
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
