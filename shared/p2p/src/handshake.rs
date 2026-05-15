use anyhow::Result;
use libp2p::{PeerId, request_response};
use log::{debug, error, warn};
use protocol::machine;
use rand::Rng;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::Instant;

use crate::envelope::{SignEnvelopeConfig, sign_with_node_keys};
use crate::message_verifier::verify_signed_message;
use crate::util::timestamp_millis;

pub const HANDSHAKE_PROTOCOL: &str = "/podmesh/handshake/1.0.0";

/// Shared, thread-safe slot holding an optional `proxy_cert_b64` value.
///
/// The proxy installs a `NodeCert` here once it has been provisioned via
/// `POST /api/v1/node_cert`. The handshake response builder reads from this
/// slot and embeds the cert in the signed handshake response so the
/// connecting sidecar can verify tenant binding.
pub type ProxyCertProvider = Arc<RwLock<Option<String>>>;

/// Build an empty (no-cert) [`ProxyCertProvider`].
pub fn empty_proxy_cert_provider() -> ProxyCertProvider {
    Arc::new(RwLock::new(None))
}

/// Tracks handshake progress per peer.
#[derive(Debug, Clone)]
pub struct HandshakeState {
    pub attempts: u8,
    pub last_attempt: Instant,
    pub confirmed: bool,
}

impl Default for HandshakeState {
    fn default() -> Self {
        Self {
            attempts: 0,
            last_attempt: Instant::now() - Duration::from_secs(3),
            confirmed: false,
        }
    }
}

/// Runtime configuration for driving outbound handshake attempts.
pub struct HandshakeDriveConfig {
    pub retry_interval: Duration,
    pub max_attempts: u8,
    pub protocol_version: &'static str,
}

impl Default for HandshakeDriveConfig {
    fn default() -> Self {
        Self {
            retry_interval: Duration::from_secs(2),
            max_attempts: 3,
            protocol_version: "podmesh/1.0",
        }
    }
}

/// Actions produced after driving pending handshakes.
#[derive(Default)]
pub struct HandshakeActions {
    pub requests: Vec<(PeerId, Vec<u8>)>,
    pub drops: Vec<PeerId>,
}

/// Ensure a peer has an entry in the handshake map and return mutable reference to its state.
pub fn track_peer<'a>(
    states: &'a mut HashMap<PeerId, HandshakeState>,
    peer: &PeerId,
) -> &'a mut HandshakeState {
    states
        .entry(peer.clone())
        .or_insert_with(HandshakeState::default)
}

/// Remove bookkeeping for the given peer.
pub fn untrack_peer(states: &mut HashMap<PeerId, HandshakeState>, peer: &PeerId) {
    states.remove(peer);
}

/// Handle an inbound `request_response` handshake message and respond via the provided closure.
pub fn handle_request_response_message(
    message: request_response::Message<Vec<u8>, Vec<u8>>,
    peer: PeerId,
    handshake_states: &mut HashMap<PeerId, HandshakeState>,
    mut send_response: impl FnMut(Vec<u8>, request_response::ResponseChannel<Vec<u8>>),
) {
    handle_request_response_message_with_cert(
        message,
        peer,
        handshake_states,
        None,
        |resp, ch| send_response(resp, ch),
    );
}

/// Variant of [`handle_request_response_message`] that allows supplying a
/// [`ProxyCertProvider`] so the response can carry an optional `proxy_cert_b64`.
pub fn handle_request_response_message_with_cert(
    message: request_response::Message<Vec<u8>, Vec<u8>>,
    peer: PeerId,
    handshake_states: &mut HashMap<PeerId, HandshakeState>,
    proxy_cert: Option<&ProxyCertProvider>,
    mut send_response: impl FnMut(Vec<u8>, request_response::ResponseChannel<Vec<u8>>),
) {
    match message {
        request_response::Message::Request {
            request, channel, ..
        } => {
            let response = handle_request(request, &peer, handshake_states, proxy_cert);
            send_response(response, channel);
        }
        request_response::Message::Response { response, .. } => {
            handle_response(response, &peer, handshake_states);
        }
    }
}

fn handle_request(
    request: Vec<u8>,
    peer: &PeerId,
    handshake_states: &mut HashMap<PeerId, HandshakeState>,
    proxy_cert: Option<&ProxyCertProvider>,
) -> Vec<u8> {
    let error_response = machine::build_handshake(0, 0, "", "");
    let verified = match verify_signed_message(peer, &request, |err| {
        error!("rejecting invalid handshake request from {peer}: {err}");
    }) {
        Some(envelope) => envelope,
        None => return error_response,
    };

    match machine::root_as_handshake(&verified.payload) {
        Ok(_) => {
            track_peer(handshake_states, peer).confirmed = true;

            let cert_b64 = proxy_cert
                .and_then(|p| p.read().ok().and_then(|guard| guard.clone()));

            if let Ok(response) = build_signed_handshake_response(peer, cert_b64.as_deref()) {
                response
            } else {
                error!("failed to sign handshake response for {peer}");
                error_response
            }
        }
        Err(e) => {
            error!("failed to parse handshake request from {peer}: {e:?}");
            error_response
        }
    }
}

fn handle_response(
    response: Vec<u8>,
    peer: &PeerId,
    handshake_states: &mut HashMap<PeerId, HandshakeState>,
) {
    let verified = match verify_signed_message(peer, &response, |err| {
        error!("rejecting invalid handshake response from {peer}: {err}");
    }) {
        Some(envelope) => envelope,
        None => return,
    };

    match machine::root_as_handshake(&verified.payload) {
        Ok(_) => {
            track_peer(handshake_states, peer).confirmed = true;
        }
        Err(e) => {
            error!("failed to parse handshake response from {peer}: {e:?}");
        }
    }
}

fn build_signed_handshake_response(peer: &PeerId, proxy_cert_b64: Option<&str>) -> Result<Vec<u8>> {
    let timestamp = timestamp_millis();
    let nonce = format!("handshake_resp_{}", rand::thread_rng().r#gen::<u32>());
    let payload = machine::build_handshake_with_cert(
        rand::random::<u32>(),
        timestamp,
        "podmesh/1.0",
        &peer.to_string(),
        proxy_cert_b64,
    );
    
    // Include our KEM public key in the handshake response so peers can encrypt messages to us
    let kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
        .ok()
        .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes));
    
    let cfg = SignEnvelopeConfig {
        nonce: Some(&nonce),
        timestamp: Some(timestamp),
        kem_pub_b64: kem_pub_b64.as_deref(),
        ..Default::default()
    };
    Ok(sign_with_node_keys(&payload, "handshake", cfg)?.bytes)
}

fn build_signed_handshake_request(
    local_peer: &PeerId,
    cfg: &HandshakeDriveConfig,
) -> Result<Vec<u8>> {
    let timestamp = timestamp_millis();
    let nonce = rand::thread_rng().r#gen::<u32>();
    let payload = machine::build_handshake(
        nonce,
        timestamp,
        cfg.protocol_version,
        &local_peer.to_string(),
    );
    let envelope_nonce = format!("handshake_req_{nonce}");
    
    // Include our KEM public key in the handshake request so peers can encrypt messages to us
    let kem_pub_b64 = crypto::ensure_kem_keypair_on_disk()
        .ok()
        .map(|(pub_bytes, _)| crypto::b64_encode(&pub_bytes));
    
    let sign_cfg = SignEnvelopeConfig {
        nonce: Some(&envelope_nonce),
        timestamp: Some(timestamp),
        kem_pub_b64: kem_pub_b64.as_deref(),
        ..Default::default()
    };
    Ok(sign_with_node_keys(&payload, "handshake", sign_cfg)?.bytes)
}

/// Build a handshake request specifically for fetching KEM public key from a peer.
/// This is exposed publicly so it can be called from the scheduler's control module.
pub fn build_handshake_request_for_kem_fetch(local_peer: &PeerId) -> Result<Vec<u8>> {
    let cfg = HandshakeDriveConfig::default();
    build_signed_handshake_request(local_peer, &cfg)
}

/// Extract KEM public key from a verified handshake response.
/// Returns the KEM pubkey as base64 string if present.
pub fn extract_kem_pubkey_from_response(
    response: &[u8],
    peer: &PeerId,
) -> Option<String> {
    let verified = verify_signed_message(peer, response, |err| {
        warn!("Failed to verify handshake response for KEM extraction: {}", err);
    })?;
    
    verified.kem_pubkey.map(|bytes| crypto::b64_encode(&bytes))
}

/// Extract `proxy_cert_b64` from a verified handshake response, returning the
/// raw base64 string if the inner payload is parseable and the field is non-empty.
///
/// Uses `verify_envelope_skip_nonce_check` so this can be called alongside the
/// standard handshake verification path without triggering nonce replay errors.
/// Signature integrity is still enforced.
pub fn extract_proxy_cert_from_response(
    response: &[u8],
    _peer: &PeerId,
) -> Option<String> {
    let verified = match crate::envelope::verify_envelope_skip_nonce_check(response) {
        Ok(v) => v,
        Err(err) => {
            warn!("Failed to verify handshake response for proxy_cert extraction: {}", err);
            return None;
        }
    };
    let handshake = machine::root_as_handshake(&verified.payload).ok()?;
    handshake.proxy_cert_b64().map(|s| s.to_string())
}

/// Drive outbound handshake attempts for every unconfirmed peer.
pub fn drive_handshakes<F, R>(
    handshake_states: &mut HashMap<PeerId, HandshakeState>,
    local_peer: &PeerId,
    cfg: &HandshakeDriveConfig,
    mut send_request: F,
    mut on_unresponsive: R,
) -> Result<()>
where
    F: FnMut(&PeerId, Vec<u8>) -> bool,
    R: FnMut(&PeerId),
{
    let now = Instant::now();
    let mut to_remove = Vec::new();

    for (peer, state) in handshake_states.iter_mut() {
        if state.confirmed {
            continue;
        }

        if state.attempts >= cfg.max_attempts {
            warn!("removing non-responsive peer {peer}");
            on_unresponsive(peer);
            to_remove.push(peer.clone());
            continue;
        }

        if state.last_attempt.elapsed() >= cfg.retry_interval {
            let request = build_signed_handshake_request(local_peer, cfg)?;
            if send_request(peer, request) {
                state.attempts += 1;
                state.last_attempt = now;
                debug!("sent handshake attempt {} to {peer}", state.attempts);
            } else {
                warn!("failed to dispatch handshake to {peer}; will retry");
            }
        }
    }

    for peer in to_remove {
        handshake_states.remove(&peer);
    }

    Ok(())
}

/// Helper that wraps [`drive_handshakes`] and collects pending requests/drops.
pub fn collect_handshake_actions(
    handshake_states: &mut HashMap<PeerId, HandshakeState>,
    local_peer: &PeerId,
    cfg: &HandshakeDriveConfig,
) -> Result<HandshakeActions> {
    let mut actions = HandshakeActions::default();
    drive_handshakes(
        handshake_states,
        local_peer,
        cfg,
        |peer, payload| {
            actions.requests.push((peer.clone(), payload));
            true
        },
        |peer| actions.drops.push(peer.clone()),
    )?;
    Ok(actions)
}
