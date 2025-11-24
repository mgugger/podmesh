use anyhow::Result;
use libp2p::{PeerId, request_response};
use log::{debug, error, warn};
use protocol::machine;
use rand::Rng;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

use crate::envelope::{SignEnvelopeConfig, sign_with_node_keys};
use crate::message_verifier::verify_signed_message;

pub const HANDSHAKE_PROTOCOL: &str = "/podmesh/handshake/1.0.0";

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
    match message {
        request_response::Message::Request {
            request, channel, ..
        } => {
            let response = handle_request(request, &peer, handshake_states);
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

            if let Ok(response) = build_signed_handshake_response(peer) {
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

fn build_signed_handshake_response(peer: &PeerId) -> Result<Vec<u8>> {
    let timestamp = current_timestamp();
    let nonce = format!("handshake_resp_{}", rand::thread_rng().r#gen::<u32>());
    let payload = machine::build_handshake(
        rand::random::<u32>(),
        timestamp,
        "podmesh/1.0",
        &peer.to_string(),
    );
    let cfg = SignEnvelopeConfig {
        nonce: Some(&nonce),
        timestamp: Some(timestamp),
        ..Default::default()
    };
    Ok(sign_with_node_keys(&payload, "handshake", cfg)?.bytes)
}

fn build_signed_handshake_request(
    local_peer: &PeerId,
    cfg: &HandshakeDriveConfig,
) -> Result<Vec<u8>> {
    let timestamp = current_timestamp();
    let nonce = rand::thread_rng().r#gen::<u32>();
    let payload = machine::build_handshake(
        nonce,
        timestamp,
        cfg.protocol_version,
        &local_peer.to_string(),
    );
    let envelope_nonce = format!("handshake_req_{nonce}");
    let sign_cfg = SignEnvelopeConfig {
        nonce: Some(&envelope_nonce),
        timestamp: Some(timestamp),
        ..Default::default()
    };
    Ok(sign_with_node_keys(&payload, "handshake", sign_cfg)?.bytes)
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
