//! The payload carried inside a workload-plane handshake envelope.
//!
//! The envelope already authenticates the sender, the recipient, the freshness
//! and the direction of the exchange, so the handshake body only carries what
//! is specific to the workload plane: the protocol version, and whichever
//! credential the two sides need to evaluate each other.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Handshake wire version. A peer refuses anything else.
pub const HANDSHAKE_PROTOCOL_VERSION: &str = "podmesh/1.0";
/// Longest credential string admitted inside a handshake.
pub const MAX_HANDSHAKE_FIELD_LEN: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadHandshakeRequest {
    pub protocol_version: String,
    pub tenant_owner_pubkey: String,
    pub manifest_id: String,
    pub workload_credential_b64: String,
}

impl WorkloadHandshakeRequest {
    pub fn new(tenant_owner_pubkey: &str, manifest_id: &str, credential: &str) -> Self {
        Self {
            protocol_version: HANDSHAKE_PROTOCOL_VERSION.to_string(),
            tenant_owner_pubkey: tenant_owner_pubkey.to_string(),
            manifest_id: manifest_id.to_string(),
            workload_credential_b64: credential.to_string(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol_version == HANDSHAKE_PROTOCOL_VERSION,
            "unsupported workload handshake protocol version"
        );
        for (name, value) in [
            ("tenant owner", self.tenant_owner_pubkey.as_str()),
            ("manifest", self.manifest_id.as_str()),
            ("credential", self.workload_credential_b64.as_str()),
        ] {
            ensure!(
                !value.is_empty() && value.len() <= MAX_HANDSHAKE_FIELD_LEN,
                "workload handshake {name} field length is invalid"
            );
        }
        Ok(())
    }
}

impl crate::WorkloadPayload for WorkloadHandshakeRequest {
    const TYPE: crate::WorkloadPayloadType = crate::WorkloadPayloadType::HandshakeRequest;

    fn validate(&self, _now_secs: u64) -> Result<()> {
        self.validate()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadHandshakeResponse {
    pub protocol_version: String,
    pub proxy_grant_b64: String,
}

impl WorkloadHandshakeResponse {
    pub fn new(proxy_grant_b64: Option<&str>) -> Self {
        Self {
            protocol_version: HANDSHAKE_PROTOCOL_VERSION.to_string(),
            proxy_grant_b64: proxy_grant_b64.unwrap_or_default().to_string(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol_version == HANDSHAKE_PROTOCOL_VERSION,
            "unsupported workload handshake protocol version"
        );
        ensure!(
            self.proxy_grant_b64.len() <= MAX_HANDSHAKE_FIELD_LEN,
            "workload handshake grant field exceeds its limit"
        );
        Ok(())
    }
}

impl crate::WorkloadPayload for WorkloadHandshakeResponse {
    const TYPE: crate::WorkloadPayloadType = crate::WorkloadPayloadType::HandshakeResponse;

    fn validate(&self, _now_secs: u64) -> Result<()> {
        self.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_fields() {
        let request = WorkloadHandshakeRequest::new("owner-key", "manifest", "credential");
        request.validate().unwrap();
        let decoded: WorkloadHandshakeRequest =
            postcard::from_bytes(&postcard::to_allocvec(&request).unwrap()).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn unsupported_version_is_refused() {
        let mut handshake = WorkloadHandshakeResponse::new(Some("grant"));
        handshake.protocol_version = "podmesh/0.9".into();
        assert!(handshake.validate().is_err());
    }

    #[test]
    fn request_and_response_have_distinct_payload_types() {
        assert_ne!(
            <WorkloadHandshakeRequest as crate::WorkloadPayload>::TYPE,
            <WorkloadHandshakeResponse as crate::WorkloadPayload>::TYPE
        );
    }
}
