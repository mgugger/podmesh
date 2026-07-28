//! Scheduler-to-scheduler relay of owner-encrypted agent control traffic.
//!
//! An agent attaches to exactly one scheduler, but `podctl` may talk to any
//! scheduler it can reach. Without this protocol a client that selected an
//! agent through gossip would be told the agent is unknown by every scheduler
//! the agent is not attached to, which makes schedulers non-interchangeable.
//!
//! The relayed payload is already signed by the namespace owner and encrypted
//! to the target agent's KEM key, so a second blind hop reveals nothing that
//! the first hop did not already carry.
//!
//! The exchange is deliberately two-phase. A deployment payload can be tens of
//! megabytes, so a scheduler first asks cheaply which peer holds the
//! attachment and only then ships the payload to that peer. Broadcasting the
//! payload to every peer would turn one client request into an N-way
//! amplification.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::IROH_ENDPOINT_ID_BYTES;
use crate::agent_control::{AgentControlOperation, MAX_AGENT_CONTROL_PAYLOAD_BYTES};

pub const AGENT_CONTROL_RELAY_ALPN: &[u8] = b"/podmesh/agent-control-relay/1";
pub const AGENT_CONTROL_RELAY_PROTOCOL_VERSION: u16 = 1;

/// Framing slack above the relayed payload: a version, a 32-byte EndpointId,
/// an intent tag, and postcard length prefixes.
const AGENT_CONTROL_RELAY_FRAME_OVERHEAD_BYTES: usize = 1024;

pub const MAX_AGENT_CONTROL_RELAY_FRAME_BYTES: usize =
    MAX_AGENT_CONTROL_PAYLOAD_BYTES + AGENT_CONTROL_RELAY_FRAME_OVERHEAD_BYTES;

/// What the calling scheduler wants from the peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentControlRelayIntent {
    /// Cheap probe: does the peer currently hold this agent's attachment?
    Locate,
    /// Deliver the carried payload to the agent and return its answer.
    Forward(AgentControlOperation),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentControlRelayRequest {
    pub version: u16,
    #[serde(with = "serde_bytes")]
    pub agent_endpoint_id: Vec<u8>,
    pub intent: AgentControlRelayIntent,
    #[serde(with = "serde_bytes")]
    pub encrypted_payload: Vec<u8>,
}

impl AgentControlRelayRequest {
    pub fn locate(agent_endpoint_id: Vec<u8>) -> Self {
        Self {
            version: AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            agent_endpoint_id,
            intent: AgentControlRelayIntent::Locate,
            encrypted_payload: Vec::new(),
        }
    }

    pub fn forward(
        agent_endpoint_id: Vec<u8>,
        operation: AgentControlOperation,
        encrypted_payload: Vec<u8>,
    ) -> Self {
        Self {
            version: AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            agent_endpoint_id,
            intent: AgentControlRelayIntent::Forward(operation),
            encrypted_payload,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode_bounded(self, "agent control relay request")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        validate_frame_size(bytes)?;
        let request: Self =
            postcard::from_bytes(bytes).context("decode agent control relay request")?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            "unsupported agent control relay protocol version"
        );
        ensure!(
            self.agent_endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "relayed agent EndpointId must contain {IROH_ENDPOINT_ID_BYTES} bytes"
        );
        match self.intent {
            AgentControlRelayIntent::Locate => ensure!(
                self.encrypted_payload.is_empty(),
                "a locate probe cannot carry a payload"
            ),
            AgentControlRelayIntent::Forward(_) => ensure!(
                !self.encrypted_payload.is_empty()
                    && self.encrypted_payload.len() <= MAX_AGENT_CONTROL_PAYLOAD_BYTES,
                "relayed agent control payload size is invalid"
            ),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentControlRelayError {
    /// The peer does not hold this agent's attachment.
    UnknownAgent,
    /// The peer is already relaying its configured maximum.
    Busy,
    /// The agent refused the owner-signed payload.
    Rejected,
    /// The peer holds the attachment but could not reach the agent.
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentControlRelayResponse {
    pub version: u16,
    pub ok: bool,
    #[serde(with = "serde_bytes")]
    pub encrypted_payload: Vec<u8>,
    pub error: Option<AgentControlRelayError>,
}

impl AgentControlRelayResponse {
    /// Positive answer to a `Locate` probe: the peer holds the attachment.
    pub fn located() -> Self {
        Self {
            version: AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            ok: true,
            encrypted_payload: Vec::new(),
            error: None,
        }
    }

    pub fn delivered(encrypted_payload: Vec<u8>) -> Self {
        Self {
            version: AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            ok: true,
            encrypted_payload,
            error: None,
        }
    }

    pub fn failed(error: AgentControlRelayError) -> Self {
        Self {
            version: AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            ok: false,
            encrypted_payload: Vec::new(),
            error: Some(error),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode_bounded(self, "agent control relay response")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        validate_frame_size(bytes)?;
        let response: Self =
            postcard::from_bytes(bytes).context("decode agent control relay response")?;
        response.validate()?;
        Ok(response)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == AGENT_CONTROL_RELAY_PROTOCOL_VERSION,
            "unsupported agent control relay response version"
        );
        ensure!(
            self.ok == self.error.is_none(),
            "agent control relay response result is ambiguous"
        );
        if self.ok {
            ensure!(
                self.encrypted_payload.len() <= MAX_AGENT_CONTROL_PAYLOAD_BYTES,
                "relayed agent control response payload is too large"
            );
        } else {
            ensure!(
                self.encrypted_payload.is_empty(),
                "a failed relay response cannot contain a payload"
            );
        }
        Ok(())
    }
}

fn encode_bounded(value: &impl Serialize, field: &str) -> Result<Vec<u8>> {
    let bytes = postcard::to_allocvec(value).with_context(|| format!("serialize {field}"))?;
    validate_frame_size(&bytes)?;
    Ok(bytes)
}

fn validate_frame_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_AGENT_CONTROL_RELAY_FRAME_BYTES,
        "agent control relay frame size is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_id() -> Vec<u8> {
        vec![9; IROH_ENDPOINT_ID_BYTES]
    }

    #[test]
    fn locate_and_forward_frames_roundtrip() {
        let locate = AgentControlRelayRequest::locate(agent_id());
        assert_eq!(
            AgentControlRelayRequest::from_bytes(&locate.to_bytes().unwrap()).unwrap(),
            locate
        );
        let forward = AgentControlRelayRequest::forward(
            agent_id(),
            AgentControlOperation::Deploy,
            vec![3; 64],
        );
        assert_eq!(
            AgentControlRelayRequest::from_bytes(&forward.to_bytes().unwrap()).unwrap(),
            forward
        );
        let located = AgentControlRelayResponse::located();
        assert_eq!(
            AgentControlRelayResponse::from_bytes(&located.to_bytes().unwrap()).unwrap(),
            located
        );
        let delivered = AgentControlRelayResponse::delivered(vec![4; 32]);
        assert_eq!(
            AgentControlRelayResponse::from_bytes(&delivered.to_bytes().unwrap()).unwrap(),
            delivered
        );
        let failed = AgentControlRelayResponse::failed(AgentControlRelayError::UnknownAgent);
        assert_eq!(
            AgentControlRelayResponse::from_bytes(&failed.to_bytes().unwrap()).unwrap(),
            failed
        );
    }

    #[test]
    fn a_locate_probe_cannot_smuggle_a_payload() {
        let mut smuggled = AgentControlRelayRequest::locate(agent_id());
        smuggled.encrypted_payload = vec![1; 8];
        assert!(smuggled.to_bytes().is_err());
    }

    #[test]
    fn a_malformed_endpoint_id_is_refused() {
        let short = AgentControlRelayRequest::locate(vec![1; IROH_ENDPOINT_ID_BYTES - 1]);
        assert!(short.to_bytes().is_err());
    }

    #[test]
    fn an_oversized_payload_is_rejected_before_encoding() {
        let oversized = AgentControlRelayRequest::forward(
            agent_id(),
            AgentControlOperation::Deploy,
            vec![0; MAX_AGENT_CONTROL_PAYLOAD_BYTES + 1],
        );
        assert!(oversized.to_bytes().is_err());
    }

    #[test]
    fn a_failed_response_cannot_carry_a_payload() {
        let mut ambiguous = AgentControlRelayResponse::failed(AgentControlRelayError::Busy);
        ambiguous.encrypted_payload = vec![1];
        assert!(ambiguous.to_bytes().is_err());
    }
}
