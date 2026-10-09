//! Workload-plane message envelope and payloads.

mod envelope;
mod messages;
mod sidecar;

pub use envelope::{
    ENVELOPE_SIGNATURE_ALGORITHM, ENVELOPE_VERSION, Envelope, EnvelopeParts, MAX_ENVELOPE_BYTES,
    MAX_ENVELOPE_DRIFT_MS,
};
pub use messages::{
    HANDSHAKE_PROTOCOL_VERSION, MAX_HANDSHAKE_FIELD_LEN, WorkloadHandshakeRequest,
    WorkloadHandshakeResponse,
};
pub use sidecar::{SidecarRouteKind, SidecarRouteSpec};
