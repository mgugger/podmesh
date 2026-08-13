//! Workload-plane message envelope and payloads.

mod envelope;
mod messages;
mod sidecar;

pub use envelope::{
    ENVELOPE_NONCE_WINDOW, ENVELOPE_SIGNATURE_ALGORITHM, ENVELOPE_VERSION, Envelope, EnvelopeParts,
    EnvelopeValidator, MAX_ENVELOPE_BYTES, MAX_ENVELOPE_DRIFT_MS,
};
pub use messages::{HANDSHAKE_PROTOCOL_VERSION, Handshake, HandshakeRole, MAX_HANDSHAKE_FIELD_LEN};
pub use sidecar::{SidecarRouteKind, SidecarRouteSpec};
