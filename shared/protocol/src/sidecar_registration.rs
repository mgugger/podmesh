use serde::{Deserialize, Serialize};

/// A single route that a sidecar serves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarRoute {
    pub path_prefix: String,
    pub port: u16,
}

/// Sent by a sidecar to the proxy on startup to register its routes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarRegistration {
    pub manifest_id: String,
    pub routes: Vec<SidecarRoute>,
    pub sidecar_peer_id: String,
    /// Base64-encoded Ed25519 owner public key (tenant key).
    pub owner_pubkey: String,
    /// Ed25519 signature over `manifest_id || sidecar_peer_id` (base64-encoded),
    /// produced with the sidecar's own Ed25519 signing key (NOT the owner key).
    pub sig: String,
    /// Base64-encoded Ed25519 public key of the sidecar (the key that produced `sig`).
    /// Allows the proxy to verify the registration signature without trusting prior state.
    pub sidecar_signing_pubkey: String,
}

impl SidecarRegistration {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SidecarRegistration serialization should succeed")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

/// Acknowledgement returned by the proxy after receiving a `SidecarRegistration`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarRegistrationAck {
    pub manifest_id: String,
    pub ok: bool,
    pub message: String,
}

impl SidecarRegistrationAck {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SidecarRegistrationAck serialization should succeed")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}
