use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{EndpointRecord, IROH_ENDPOINT_ID_BYTES, WorkloadListRequest};

pub const RECONCILIATION_PROTOCOL_VERSION: u16 = 1;
pub const SCHEDULER_RECONCILIATION_ALPN: &[u8] = b"/podmesh/scheduler-reconciliation/1";
pub const MAX_RECONCILIATION_LIFETIME_SECS: u64 = 15;
pub const MAX_RECONCILIATION_CLOCK_SKEW_SECS: u64 = 5;
pub const MAX_RECONCILIATION_QUERY_ID_BYTES: usize = 128;
pub const MAX_RECONCILIATION_RESPONSE_BYTES: usize =
    crate::MAX_AGENT_CONTROL_PAYLOAD_BYTES + 4 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerReconciliationQuery {
    pub version: u16,
    pub query_id: String,
    #[serde(with = "serde_bytes")]
    pub owner_request: Vec<u8>,
    pub reply_endpoint: EndpointRecord,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signing_pubkey: String,
    pub signature: String,
}

impl SchedulerReconciliationQuery {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(&Self {
            signature: String::new(),
            ..self.clone()
        })
        .context("serialize canonical reconciliation query")
    }

    pub fn sign(
        mut self,
        signing_public: &[u8],
        signing_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.signing_pubkey = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::SchedulerReconciliationQuery,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        let public = crypto::b64_decode(&self.signing_pubkey)?;
        let signature = crypto::b64_decode(&self.signature)?;
        crypto::verify_domain(
            &public,
            crypto::SignatureDomain::SchedulerReconciliationQuery,
            &self.canonical_bytes()?,
            &signature,
        )
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == RECONCILIATION_PROTOCOL_VERSION,
            "unsupported reconciliation query version"
        );
        validate_query_id(&self.query_id)?;
        validate_window(self.issued_at_secs, self.expires_at_secs, now_secs)?;
        WorkloadListRequest::from_bytes(&self.owner_request, now_secs)?;
        self.reply_endpoint.verify(now_secs)?;
        ensure!(
            self.reply_endpoint.signing_pubkey == self.signing_pubkey,
            "reconciliation reply endpoint does not belong to the query signer"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReconciliationEvent {
    AgentAnswer {
        #[serde(with = "serde_bytes")]
        agent_endpoint_id: Vec<u8>,
        #[serde(with = "serde_bytes")]
        sealed_response: Vec<u8>,
    },
    AgentUnreachable {
        #[serde(with = "serde_bytes")]
        agent_endpoint_id: Vec<u8>,
    },
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerReconciliationResponse {
    pub version: u16,
    pub query_id: String,
    pub responder_endpoint: EndpointRecord,
    pub event: ReconciliationEvent,
    pub responded_at_secs: u64,
    pub signing_pubkey: String,
    pub signature: String,
}

impl SchedulerReconciliationResponse {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(&Self {
            signature: String::new(),
            ..self.clone()
        })
        .context("serialize canonical reconciliation response")
    }

    pub fn sign(
        mut self,
        signing_public: &[u8],
        signing_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.signing_pubkey = crypto::b64_encode(signing_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_domain(
            signing_private,
            crypto::SignatureDomain::SchedulerReconciliationResponse,
            &self.canonical_bytes()?,
        )?);
        Ok(self)
    }

    pub fn verify(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        let public = crypto::b64_decode(&self.signing_pubkey)?;
        let signature = crypto::b64_decode(&self.signature)?;
        crypto::verify_domain(
            &public,
            crypto::SignatureDomain::SchedulerReconciliationResponse,
            &self.canonical_bytes()?,
            &signature,
        )
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.verify(now_secs)?;
        let bytes = postcard::to_allocvec(self).context("serialize reconciliation response")?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_RECONCILIATION_RESPONSE_BYTES,
            "reconciliation response size is invalid"
        );
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_RECONCILIATION_RESPONSE_BYTES,
            "reconciliation response size is invalid"
        );
        let response: Self =
            postcard::from_bytes(bytes).context("decode reconciliation response")?;
        response.verify(now_secs)?;
        Ok(response)
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == RECONCILIATION_PROTOCOL_VERSION,
            "unsupported reconciliation response version"
        );
        validate_query_id(&self.query_id)?;
        self.responder_endpoint.verify(now_secs)?;
        ensure!(
            self.responder_endpoint.signing_pubkey == self.signing_pubkey,
            "reconciliation responder endpoint does not belong to the signer"
        );
        ensure!(
            self.responded_at_secs <= now_secs.saturating_add(MAX_RECONCILIATION_CLOCK_SKEW_SECS),
            "reconciliation response was issued too far in the future"
        );
        match &self.event {
            ReconciliationEvent::AgentAnswer {
                agent_endpoint_id,
                sealed_response,
            } => {
                validate_endpoint_id(agent_endpoint_id)?;
                ensure!(
                    !sealed_response.is_empty()
                        && sealed_response.len() <= crate::MAX_AGENT_CONTROL_PAYLOAD_BYTES,
                    "sealed reconciliation answer size is invalid"
                );
            }
            ReconciliationEvent::AgentUnreachable { agent_endpoint_id } => {
                validate_endpoint_id(agent_endpoint_id)?;
            }
            ReconciliationEvent::Complete => {}
        }
        Ok(())
    }
}

fn validate_query_id(query_id: &str) -> Result<()> {
    ensure!(
        !query_id.is_empty() && query_id.len() <= MAX_RECONCILIATION_QUERY_ID_BYTES,
        "reconciliation query id length is invalid"
    );
    Ok(())
}

fn validate_endpoint_id(endpoint_id: &[u8]) -> Result<()> {
    ensure!(
        endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
        "reconciliation EndpointId must contain {IROH_ENDPOINT_ID_BYTES} bytes"
    );
    Ok(())
}

fn validate_window(issued_at_secs: u64, expires_at_secs: u64, now_secs: u64) -> Result<()> {
    ensure!(
        issued_at_secs <= now_secs.saturating_add(MAX_RECONCILIATION_CLOCK_SKEW_SECS),
        "reconciliation query was issued too far in the future"
    );
    ensure!(
        expires_at_secs > issued_at_secs
            && expires_at_secs - issued_at_secs <= MAX_RECONCILIATION_LIFETIME_SECS,
        "reconciliation query lifetime is invalid"
    );
    ensure!(
        expires_at_secs.saturating_add(MAX_RECONCILIATION_CLOCK_SKEW_SECS) >= now_secs,
        "reconciliation query expired"
    );
    Ok(())
}
