//! The payload carried inside a workload-plane handshake envelope.
//!
//! The envelope already authenticates the sender, the recipient, the freshness
//! and the direction of the exchange, so the handshake body only carries what
//! is specific to the workload plane: the protocol version, and whichever
//! credential the two sides need to evaluate each other.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// Handshake wire version. A peer refuses anything else.
pub const HANDSHAKE_PROTOCOL_VERSION: &str = "podmesh/1.0";
/// Longest credential string admitted inside a handshake.
pub const MAX_HANDSHAKE_FIELD_LEN: usize = 16 * 1024;

/// Which side of the exchange a handshake is. Carried in the envelope payload
/// type, so a response can never be replayed as a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeRole {
    /// Sidecar opening a session with a proxy.
    Request,
    /// Proxy answering a sidecar.
    Response,
}

impl HandshakeRole {
    pub const fn payload_type(self) -> &'static str {
        match self {
            Self::Request => "podmesh/workload-handshake-request",
            Self::Response => "podmesh/workload-handshake-response",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    pub protocol_version: String,
    /// Base64 Ed25519 key of the namespace owner, sent by the sidecar so the
    /// proxy knows which tenant grant to present.
    ///
    /// This is only a claim. On its own it settles nothing, because the key is
    /// public; `workload_credential_b64` is what makes it worth believing.
    pub tenant_owner_pubkey: String,
    /// Routing key of the workload the sidecar serves, claimed alongside the
    /// owner and proven by the same credential.
    pub manifest_id: String,
    /// Base64 Biscuit the sidecar presents to prove its owner deployed it.
    ///
    /// Signed by the owner's key, so naming a tenant is worthless without that
    /// tenant's private key.
    pub workload_credential_b64: String,
    /// Base64 Biscuit the proxy presents to prove the owner authorised it.
    pub proxy_grant_b64: String,
}

impl Handshake {
    pub fn request(
        tenant_owner_pubkey: Option<&str>,
        manifest_id: Option<&str>,
        workload_credential_b64: Option<&str>,
    ) -> Self {
        Self {
            protocol_version: HANDSHAKE_PROTOCOL_VERSION.to_string(),
            tenant_owner_pubkey: tenant_owner_pubkey.unwrap_or_default().to_string(),
            manifest_id: manifest_id.unwrap_or_default().to_string(),
            workload_credential_b64: workload_credential_b64.unwrap_or_default().to_string(),
            proxy_grant_b64: String::new(),
        }
    }

    pub fn response(proxy_grant_b64: Option<&str>) -> Self {
        Self {
            protocol_version: HANDSHAKE_PROTOCOL_VERSION.to_string(),
            tenant_owner_pubkey: String::new(),
            manifest_id: String::new(),
            workload_credential_b64: String::new(),
            proxy_grant_b64: proxy_grant_b64.unwrap_or_default().to_string(),
        }
    }

    pub fn tenant_owner_pubkey(&self) -> Option<&str> {
        non_empty(&self.tenant_owner_pubkey)
    }

    pub fn manifest_id(&self) -> Option<&str> {
        non_empty(&self.manifest_id)
    }

    pub fn workload_credential_b64(&self) -> Option<&str> {
        non_empty(&self.workload_credential_b64)
    }

    pub fn proxy_grant_b64(&self) -> Option<&str> {
        non_empty(&self.proxy_grant_b64)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        postcard::to_allocvec(self).context("serialize handshake")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let handshake: Self = postcard::from_bytes(bytes).context("decode handshake")?;
        handshake.validate()?;
        Ok(handshake)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol_version == HANDSHAKE_PROTOCOL_VERSION,
            "unsupported workload handshake protocol version {:?}",
            self.protocol_version
        );
        ensure!(
            self.tenant_owner_pubkey.len() <= MAX_HANDSHAKE_FIELD_LEN
                && self.manifest_id.len() <= MAX_HANDSHAKE_FIELD_LEN
                && self.workload_credential_b64.len() <= MAX_HANDSHAKE_FIELD_LEN
                && self.proxy_grant_b64.len() <= MAX_HANDSHAKE_FIELD_LEN,
            "workload handshake field exceeds {MAX_HANDSHAKE_FIELD_LEN} bytes"
        );
        Ok(())
    }
}

fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_fields() {
        let request = Handshake::request(Some("owner-key"), Some("manifest"), Some("credential"));
        let decoded = Handshake::from_bytes(&request.to_bytes().unwrap()).unwrap();
        assert_eq!(decoded.tenant_owner_pubkey(), Some("owner-key"));
        assert_eq!(decoded.manifest_id(), Some("manifest"));
        assert_eq!(decoded.workload_credential_b64(), Some("credential"));
        assert_eq!(decoded.proxy_grant_b64(), None);
    }

    #[test]
    fn unsupported_version_is_refused() {
        let mut handshake = Handshake::response(Some("grant"));
        handshake.protocol_version = "podmesh/0.9".into();
        let bytes = postcard::to_allocvec(&handshake).unwrap();
        assert!(Handshake::from_bytes(&bytes).is_err());
    }

    #[test]
    fn roles_have_distinct_payload_types() {
        assert_ne!(
            HandshakeRole::Request.payload_type(),
            HandshakeRole::Response.payload_type()
        );
    }
}
