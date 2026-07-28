use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const AGENT_CONTROL_ALPN: &[u8] = b"/podmesh/agent-control/1";
pub const AGENT_CONTROL_PROTOCOL_VERSION: u16 = 1;
pub const MAX_AGENT_CONTROL_PAYLOAD_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_AGENT_CONTROL_FRAME_BYTES: usize = MAX_AGENT_CONTROL_PAYLOAD_BYTES + 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentControlOperation {
    Admission,
    Deploy,
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentControlRequest {
    pub version: u16,
    pub operation: AgentControlOperation,
    #[serde(with = "serde_bytes")]
    pub encrypted_payload: Vec<u8>,
}

impl AgentControlRequest {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode_bounded(self, "agent control request")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        validate_frame_size(bytes)?;
        let request: Self = postcard::from_bytes(bytes).context("decode agent control request")?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == AGENT_CONTROL_PROTOCOL_VERSION,
            "unsupported agent control protocol version"
        );
        validate_payload(&self.encrypted_payload)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentControlError {
    Rejected,
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentControlResponse {
    pub version: u16,
    pub ok: bool,
    #[serde(with = "serde_bytes")]
    pub encrypted_payload: Vec<u8>,
    pub error: Option<AgentControlError>,
}

impl AgentControlResponse {
    pub fn success(encrypted_payload: Vec<u8>) -> Self {
        Self {
            version: AGENT_CONTROL_PROTOCOL_VERSION,
            ok: true,
            encrypted_payload,
            error: None,
        }
    }

    pub fn rejected() -> Self {
        Self {
            version: AGENT_CONTROL_PROTOCOL_VERSION,
            ok: false,
            encrypted_payload: Vec::new(),
            error: Some(AgentControlError::Rejected),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode_bounded(self, "agent control response")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        validate_frame_size(bytes)?;
        let response: Self =
            postcard::from_bytes(bytes).context("decode agent control response")?;
        response.validate()?;
        Ok(response)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == AGENT_CONTROL_PROTOCOL_VERSION,
            "unsupported agent control response version"
        );
        ensure!(
            self.ok == self.error.is_none(),
            "agent control response result is ambiguous"
        );
        if self.ok {
            validate_payload(&self.encrypted_payload)?;
        } else {
            ensure!(
                self.encrypted_payload.is_empty(),
                "rejected agent response cannot contain a payload"
            );
        }
        Ok(())
    }
}

fn validate_payload(payload: &[u8]) -> Result<()> {
    ensure!(
        !payload.is_empty() && payload.len() <= MAX_AGENT_CONTROL_PAYLOAD_BYTES,
        "agent control payload size is invalid"
    );
    Ok(())
}

fn encode_bounded(value: &impl Serialize, field: &str) -> Result<Vec<u8>> {
    let bytes = postcard::to_allocvec(value).with_context(|| format!("serialize {field}"))?;
    validate_frame_size(&bytes)?;
    Ok(bytes)
}

fn validate_frame_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_AGENT_CONTROL_FRAME_BYTES,
        "agent control frame size is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_frames_roundtrip_and_reject_ambiguity() {
        let request = AgentControlRequest {
            version: AGENT_CONTROL_PROTOCOL_VERSION,
            operation: AgentControlOperation::Admission,
            encrypted_payload: vec![7; 32],
        };
        assert_eq!(
            AgentControlRequest::from_bytes(&request.to_bytes().unwrap()).unwrap(),
            request
        );
        let response = AgentControlResponse::success(vec![8; 32]);
        assert_eq!(
            AgentControlResponse::from_bytes(&response.to_bytes().unwrap()).unwrap(),
            response
        );
        let ambiguous = AgentControlResponse {
            version: AGENT_CONTROL_PROTOCOL_VERSION,
            ok: true,
            encrypted_payload: vec![1],
            error: Some(AgentControlError::Rejected),
        };
        assert!(ambiguous.to_bytes().is_err());
    }

    #[test]
    fn oversized_payload_is_rejected_before_encoding() {
        let request = AgentControlRequest {
            version: AGENT_CONTROL_PROTOCOL_VERSION,
            operation: AgentControlOperation::Deploy,
            encrypted_payload: vec![0; MAX_AGENT_CONTROL_PAYLOAD_BYTES + 1],
        };
        assert!(request.to_bytes().is_err());
    }
}
