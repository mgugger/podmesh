use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use protocol::machine::{self, Handshake};
use uuid::Uuid;

const HANDSHAKE_PROTOCOL_VERSION: &str = "podmesh/1.0";
const HANDSHAKE_PAYLOAD_TYPE: &str = "handshake";
const SIGNATURE_ALGORITHM: &str = "ed25519";
const SIGNATURE_PREFIX: &str = "ed25519";
const MAX_TIMESTAMP_DRIFT_MS: u64 = 90 * 1_000;
const NONCE_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug)]
pub struct VerifiedWorkloadHandshake {
    pub handshake: Handshake,
    pub signing_pubkey: Vec<u8>,
    pub kem_pubkey: Option<Vec<u8>>,
}

pub fn build_workload_handshake_request(
    local_endpoint: EndpointId,
    tenant_owner_pubkey: Option<&str>,
) -> Result<Vec<u8>> {
    build_signed_handshake(local_endpoint, tenant_owner_pubkey, None)
}

pub fn build_workload_handshake_response(
    local_endpoint: EndpointId,
    proxy_grant_b64: Option<&str>,
) -> Result<Vec<u8>> {
    build_signed_handshake(local_endpoint, None, proxy_grant_b64)
}

pub fn verify_workload_handshake(
    bytes: &[u8],
    remote_endpoint: EndpointId,
) -> Result<VerifiedWorkloadHandshake> {
    let envelope =
        machine::root_as_envelope(bytes).context("decode workload handshake envelope")?;
    ensure!(
        envelope.payload_type() == Some(HANDSHAKE_PAYLOAD_TYPE),
        "unexpected workload handshake payload type"
    );
    ensure!(
        envelope.alg() == Some(SIGNATURE_ALGORITHM),
        "unsupported workload handshake signature algorithm"
    );
    let remote = remote_endpoint.to_string();
    ensure!(
        envelope.peer_id() == Some(remote.as_str()),
        "workload handshake endpoint binding does not match transport"
    );
    validate_timestamp(envelope.ts())?;

    let nonce = envelope.nonce().unwrap_or_default();
    ensure!(!nonce.is_empty(), "workload handshake nonce is missing");
    crypto::nonce_helper::check_and_insert_nonce_for_peer(nonce, NONCE_WINDOW, &remote)?;

    let payload = envelope.payload().unwrap_or_default();
    let canonical = machine::build_envelope_canonical_with_peer(
        payload,
        HANDSHAKE_PAYLOAD_TYPE,
        nonce,
        envelope.ts(),
        SIGNATURE_ALGORITHM,
        &remote,
        envelope.kem_pubkey(),
    );
    let signing_pubkey = crypto::b64_decode(envelope.pubkey().unwrap_or_default())
        .context("decode workload handshake signing key")?;
    let signature = crypto::nonce_helper::normalize_and_decode_signature(envelope.sig())?;
    crypto::verify_envelope(&signing_pubkey, &canonical, &signature)
        .context("verify workload handshake signature")?;

    let handshake = machine::root_as_handshake(payload).context("decode workload handshake")?;
    ensure!(
        handshake.protocol_version() == Some(HANDSHAKE_PROTOCOL_VERSION),
        "unsupported workload handshake protocol version"
    );
    ensure!(
        handshake.signature() == Some(remote.as_str()),
        "workload handshake identity does not match transport"
    );
    let kem_pubkey = envelope
        .kem_pubkey()
        .filter(|value| !value.is_empty())
        .map(crypto::b64_decode)
        .transpose()
        .context("decode workload handshake KEM key")?;

    Ok(VerifiedWorkloadHandshake {
        handshake,
        signing_pubkey,
        kem_pubkey,
    })
}

fn build_signed_handshake(
    local_endpoint: EndpointId,
    tenant_owner_pubkey: Option<&str>,
    proxy_grant_b64: Option<&str>,
) -> Result<Vec<u8>> {
    let now = now_millis()?;
    let message_id = Uuid::new_v4();
    let nonce_bytes: [u8; 4] = message_id.as_bytes()[..4]
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid handshake nonce source"))?;
    let local = local_endpoint.to_string();
    let payload = match (tenant_owner_pubkey, proxy_grant_b64) {
        (Some(owner), None) => machine::build_handshake_with_tenant(
            u32::from_be_bytes(nonce_bytes),
            now,
            HANDSHAKE_PROTOCOL_VERSION,
            &local,
            owner,
        ),
        (None, cert) => machine::build_handshake_with_grant(
            u32::from_be_bytes(nonce_bytes),
            now,
            HANDSHAKE_PROTOCOL_VERSION,
            &local,
            cert,
        ),
        (Some(_), Some(_)) => {
            anyhow::bail!("handshake cannot contain tenant and proxy certificate")
        }
    };
    let (signing_public, signing_private) = crypto::ensure_keypair_on_disk()?;
    let kem_public = crypto::ensure_kem_keypair_on_disk()
        .ok()
        .map(|(public, _)| crypto::b64_encode(&public));
    let nonce = message_id.to_string();
    let canonical = machine::build_envelope_canonical_with_peer(
        &payload,
        HANDSHAKE_PAYLOAD_TYPE,
        &nonce,
        now,
        SIGNATURE_ALGORITHM,
        &local,
        kem_public.as_deref(),
    );
    let (signature, public_key) =
        crypto::sign_envelope(&signing_private, &signing_public, &canonical)?;
    Ok(machine::build_envelope_signed(
        machine::SignedEnvelopeParams {
            payload: &payload,
            payload_type: HANDSHAKE_PAYLOAD_TYPE,
            nonce: &nonce,
            timestamp: now,
            algorithm: SIGNATURE_ALGORITHM,
            signature_prefix: SIGNATURE_PREFIX,
            signature_b64: &signature,
            public_key_b64: &public_key,
            peer_id: Some(&local),
            kem_public_key_b64: kem_public.as_deref(),
        },
    ))
}

fn validate_timestamp(timestamp: u64) -> Result<()> {
    let now = now_millis()?;
    let normalized = if timestamp < now / 100 {
        timestamp
            .checked_mul(1_000)
            .ok_or_else(|| anyhow::anyhow!("workload handshake timestamp overflow"))?
    } else {
        timestamp
    };
    ensure!(
        normalized.abs_diff(now) <= MAX_TIMESTAMP_DRIFT_MS,
        "workload handshake timestamp is outside the allowed window"
    );
    Ok(())
}

fn now_millis() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_millis()
        .try_into()
        .context("system time exceeds u64 milliseconds")?)
}
