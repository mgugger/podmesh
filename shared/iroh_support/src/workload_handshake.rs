//! Mutual handshake on the workload plane.
//!
//! Both directions travel in a signed [`Envelope`] that names the sender, the
//! intended recipient, and the direction of the exchange. Because the recipient
//! and the role are inside the signature, a captured handshake cannot be
//! forwarded to a third peer, and a response cannot be replayed as a request.
//!
//! Identity keys are passed in explicitly rather than read from a process-wide
//! location, so every component in a process — and every component in a test —
//! signs with its own key.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use protocol::machine::{Envelope, EnvelopeParts, EnvelopeValidator, Handshake, HandshakeRole};
use uuid::Uuid;

/// The identity a component signs its handshakes with.
#[derive(Clone)]
pub struct HandshakeIdentity {
    pub signing_public: Vec<u8>,
    pub signing_private: Vec<u8>,
    pub kem_public: Option<Vec<u8>>,
}

/// A handshake whose envelope passed every validation rule.
#[derive(Clone, Debug)]
pub struct VerifiedWorkloadHandshake {
    pub handshake: Handshake,
    pub signing_pubkey: Vec<u8>,
    pub kem_pubkey: Option<Vec<u8>>,
}

/// Build the sidecar's opening handshake, addressed to a specific proxy.
pub fn build_workload_handshake_request(
    identity: &HandshakeIdentity,
    local_endpoint: EndpointId,
    remote_endpoint: EndpointId,
    tenant_owner_pubkey: Option<&str>,
    manifest_id: Option<&str>,
    workload_credential_b64: Option<&str>,
) -> Result<Vec<u8>> {
    seal(
        identity,
        local_endpoint,
        remote_endpoint,
        HandshakeRole::Request,
        &Handshake::request(tenant_owner_pubkey, manifest_id, workload_credential_b64),
    )
}

/// Build the proxy's answer, addressed to the sidecar that opened the session.
pub fn build_workload_handshake_response(
    identity: &HandshakeIdentity,
    local_endpoint: EndpointId,
    remote_endpoint: EndpointId,
    proxy_grant_b64: Option<&str>,
) -> Result<Vec<u8>> {
    seal(
        identity,
        local_endpoint,
        remote_endpoint,
        HandshakeRole::Response,
        &Handshake::response(proxy_grant_b64),
    )
}

/// Validate a handshake received from `remote_endpoint` and addressed to
/// `local_endpoint`, in the expected direction.
pub fn verify_workload_handshake(
    bytes: &[u8],
    local_endpoint: EndpointId,
    remote_endpoint: EndpointId,
    expected_role: HandshakeRole,
) -> Result<VerifiedWorkloadHandshake> {
    let envelope = EnvelopeValidator::new(
        &local_endpoint.to_string(),
        &remote_endpoint.to_string(),
        now_millis()?,
    )
    .accept(bytes, expected_role.payload_type())
    .context("validate workload handshake envelope")?;

    let handshake =
        Handshake::from_bytes(&envelope.payload).context("decode workload handshake payload")?;

    Ok(VerifiedWorkloadHandshake {
        handshake,
        signing_pubkey: envelope.sender_signing_key()?,
        kem_pubkey: envelope.sender_kem_key()?,
    })
}

fn seal(
    identity: &HandshakeIdentity,
    local_endpoint: EndpointId,
    remote_endpoint: EndpointId,
    role: HandshakeRole,
    handshake: &Handshake,
) -> Result<Vec<u8>> {
    ensure!(
        local_endpoint != remote_endpoint,
        "a workload handshake cannot be addressed to its own endpoint"
    );
    let payload = handshake.to_bytes()?;
    Envelope::seal(
        EnvelopeParts {
            payload: &payload,
            payload_type: role.payload_type(),
            nonce: &Uuid::new_v4().to_string(),
            ts_millis: now_millis()?,
            sender_id: &local_endpoint.to_string(),
            recipient_id: &remote_endpoint.to_string(),
            sender_signing_pubkey: &identity.signing_public,
            sender_kem_pubkey: identity.kem_public.as_deref(),
        },
        &identity.signing_private,
    )?
    .to_bytes()
}

fn now_millis() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_millis()
        .try_into()
        .context("system time exceeds u64 milliseconds")
}
