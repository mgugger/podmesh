use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{EndpointRecord, MachineRole};

pub const SCHEDULER_MESH_PROTOCOL_VERSION: u16 = 1;
pub const AGENT_CAPACITY_ALPN: &[u8] = b"/podmesh/agent-capacity/1";
pub const CAPACITY_OFFER_ALPN: &[u8] = b"/podmesh/capacity-offer/1";
pub const SCHEDULER_PLACEMENT_ALPN: &[u8] = b"/podmesh/scheduler-placement/1";
pub const MAX_AGENT_ATTACHMENT_BYTES: usize = 8 * 1024;
pub const MAX_AGENT_ATTACHMENT_LIFETIME_SECS: u64 = 60;
pub const MAX_AGENT_ATTACHMENT_NONCE_LEN: usize = 128;
pub const MAX_AGENT_ATTACHMENT_CLOCK_SKEW_SECS: u64 = 5;
pub const MAX_ATTACHMENT_RELAY_GRANTS: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAttachmentHello {
    pub version: u16,
    pub role: MachineRole,
    pub agent_endpoint: EndpointRecord,
    pub nonce: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signing_pubkey: String,
    pub signature: String,
}

impl AgentAttachmentHello {
    pub fn sign(
        mut self,
        signing_public: &[u8],
        signing_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.signing_pubkey = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            signing_private,
            &self.canonical_bytes()?,
        )?);
        self.validate(now_secs)?;
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> Result<()> {
        self.validate(now_secs)?;
        let public = crypto::b64_decode(&self.signing_pubkey)?;
        let signature = crypto::b64_decode(&self.signature)?;
        crypto::verify_envelope(&public, &self.canonical_bytes()?, &signature)
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.verify(now_secs)?;
        encode_bounded(self)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_size(bytes)?;
        let hello: Self = postcard::from_bytes(bytes).context("decode agent attachment hello")?;
        hello.verify(now_secs)?;
        Ok(hello)
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        encode_bounded(&Self {
            signature: String::new(),
            ..self.clone()
        })
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        ensure!(
            crypto::b64_decode(&self.signing_pubkey)?.len() == 32,
            "agent attachment signing key must decode to 32 bytes"
        );
        ensure!(
            crypto::b64_decode(&self.signature)?.len() == 64,
            "agent attachment signature must decode to 64 bytes"
        );
        Ok(())
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == SCHEDULER_MESH_PROTOCOL_VERSION,
            "unsupported agent attachment version"
        );
        ensure!(
            self.role == MachineRole::Agent,
            "agent attachment requires the agent role"
        );
        self.agent_endpoint.verify(now_secs)?;
        ensure!(
            self.agent_endpoint.signing_pubkey == self.signing_pubkey,
            "agent attachment signer is not bound to endpoint record"
        );
        ensure!(
            !self.nonce.is_empty() && self.nonce.len() <= MAX_AGENT_ATTACHMENT_NONCE_LEN,
            "agent attachment nonce length is invalid"
        );
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_AGENT_ATTACHMENT_CLOCK_SKEW_SECS),
            "agent attachment issue time is too far in the future"
        );
        ensure!(self.expires_at_secs >= now_secs, "agent attachment expired");
        ensure!(
            self.expires_at_secs >= self.issued_at_secs
                && self.expires_at_secs - self.issued_at_secs <= MAX_AGENT_ATTACHMENT_LIFETIME_SECS,
            "agent attachment lifetime is invalid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAttachmentAck {
    pub version: u16,
    pub relay_grants: Vec<String>,
    pub refresh_after_secs: u64,
}

impl AgentAttachmentAck {
    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.validate(now_secs)?;
        encode_bounded(self)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_size(bytes)?;
        let ack: Self = postcard::from_bytes(bytes).context("decode agent attachment ack")?;
        ack.validate(now_secs)?;
        Ok(ack)
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == SCHEDULER_MESH_PROTOCOL_VERSION,
            "unsupported agent attachment acknowledgement version"
        );
        ensure!(
            self.relay_grants.len() <= MAX_ATTACHMENT_RELAY_GRANTS,
            "too many attachment relay grants"
        );
        ensure!(
            self.refresh_after_secs > now_secs,
            "agent attachment refresh deadline must be in the future"
        );
        for grant in &self.relay_grants {
            ensure!(
                !grant.is_empty() && grant.len() <= crate::MAX_MACHINE_RELAY_AUTH_TOKEN_LEN,
                "attachment relay grant length is invalid"
            );
        }
        Ok(())
    }
}

fn encode_bounded(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = postcard::to_allocvec(value).context("serialize agent attachment hello")?;
    validate_size(&bytes)?;
    Ok(bytes)
}

fn validate_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_AGENT_ATTACHMENT_BYTES,
        "agent attachment encoded size is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{ENDPOINT_RECORD_VERSION, MachineRole};

    use super::*;

    #[test]
    fn attachment_signature_binds_agent_role() {
        let now = 1_000;
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
        let endpoint = EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: vec![7; 32],
            relay_url: None,
            direct_addresses: vec!["127.0.0.1:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 60,
            signature: String::new(),
        }
        .sign(&public, &private, now)
        .unwrap();
        let hello = AgentAttachmentHello {
            version: SCHEDULER_MESH_PROTOCOL_VERSION,
            role: MachineRole::Agent,
            agent_endpoint: endpoint,
            nonce: "nonce".into(),
            issued_at_secs: now,
            expires_at_secs: now + 30,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(&public, &private, now)
        .unwrap();
        let mut changed = hello;
        changed.role = MachineRole::Scheduler;
        assert!(changed.verify(now).is_err());
    }
}
