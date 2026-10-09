pub mod agent;
pub use agent::{
    AGENT_PROTOCOL_VERSION, AdmissionRequest, DeploymentGrant, DeploymentReceipt,
    EncryptedWorkloadCapsule, ExecutionSpec, MAX_WORKLOAD_LIST_REQUEST_BYTES,
    MAX_WORKLOAD_REPLICAS, Reservation, UpdateOutcome, UpdateRequest, UpdateResponse,
    WorkloadCommand, WorkloadCommandResponse, WorkloadListRequest, WorkloadListResponse,
    WorkloadOperation, WorkloadSummary, deployment_id, revision_id, workload_id,
};
pub mod agent_control;
pub use agent_control::{
    AGENT_CONTROL_ALPN, AGENT_CONTROL_PROTOCOL_VERSION, AgentControlError, AgentControlOperation,
    AgentControlRequest, AgentControlResponse, MAX_AGENT_CONTROL_FRAME_BYTES,
    MAX_AGENT_CONTROL_PAYLOAD_BYTES,
};

pub mod agent_control_relay;
pub use agent_control_relay::{
    AGENT_CONTROL_RELAY_ALPN, AGENT_CONTROL_RELAY_PROTOCOL_VERSION, AgentControlRelayError,
    AgentControlRelayIntent, AgentControlRelayRequest, AgentControlRelayResponse,
    MAX_AGENT_CONTROL_RELAY_FRAME_BYTES,
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
pub use http_proxy::{
    IngressRequestMetadata, IngressResponseMetadata, ProxyHttpRequest, ProxyHttpResponse,
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
pub use sidecar_registration::{
    MAX_SIDECAR_ROUTES, SIDECAR_REGISTRATION_VERSION, SidecarRegistration, SidecarRegistrationAck,
    SidecarRoute, route_id,
};
pub mod scheduler_gossip;
pub use scheduler_gossip::{
    AgentLocationQuery, MAX_LOCATION_QUERY_LIFETIME_SECS, SCHEDULER_GOSSIP_PROTOCOL_VERSION,
    SchedulerGossipMessage,
};
pub mod reconciliation;
#[cfg(test)]
mod reconciliation_tests;
pub use reconciliation::{
    MAX_RECONCILIATION_LIFETIME_SECS, MAX_RECONCILIATION_RESPONSE_BYTES,
    RECONCILIATION_PROTOCOL_VERSION, ReconciliationEvent, SCHEDULER_RECONCILIATION_ALPN,
    SchedulerReconciliationQuery, SchedulerReconciliationResponse,
};
pub mod scheduler_mesh;
pub use scheduler_mesh::{
    AGENT_CAPACITY_ALPN, AgentAttachmentAck, AgentAttachmentHello, CAPACITY_OFFER_ALPN,
    MAX_AGENT_ATTACHMENT_BYTES, SCHEDULER_MESH_PROTOCOL_VERSION, SCHEDULER_PLACEMENT_ALPN,
};
pub mod biscuit_keys;
pub use biscuit_keys::{
    MAX_BISCUIT_TOKEN_BYTES, biscuit_keypair_from_ed25519, biscuit_public_key_from_ed25519,
};
pub mod proxy_grant;
pub mod replay_registry;
pub use replay_registry::{PeerReplayRegistry, ReplayLimits};
pub mod relay_token;
pub mod workload_body;
pub mod workload_credential;
pub use workload_body::{
    HttpBodyProgress, finish_http_body, read_http_body_chunk, write_http_body_chunk,
};
pub mod workload_control;
pub use proxy_grant::{
    MAX_PROXY_GRANT_B64_LEN, MAX_PROXY_GRANT_LIFETIME_SECS, ProxyGrantClaims, mint_proxy_grant,
    proxy_grant_from_b64, proxy_grant_to_b64, verify_proxy_grant,
};
pub use relay_token::{derive_tenant_relay_token, tenant_from_relay_token};
pub use workload_control::{
    AcceptedWorkloadPayload, ProxyAnnouncementRequest, ProxyAnnouncementResponse,
    WorkloadEnvelopeParts, WorkloadPayload, WorkloadPayloadType, WorkloadStreamPhase,
    accept_workload_payload, seal_workload_payload,
};
pub use workload_credential::{
    MAX_WORKLOAD_CREDENTIAL_B64_LEN, MAX_WORKLOAD_CREDENTIAL_LIFETIME_SECS,
    WorkloadCredentialClaims, mint_workload_credential, verify_workload_credential,
    verify_workload_credential_structure, workload_credential_from_b64, workload_credential_to_b64,
};
pub mod workload_stream;
pub use workload_stream::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, MAX_WORKLOAD_STREAMS_PER_CONNECTION, MESH_DOMAIN_SUFFIX,
    WORKLOAD_ALPN, WorkloadStreamKind, read_workload_frame, write_workload_frame,
};
