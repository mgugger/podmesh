use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::IROH_ENDPOINT_ID_BYTES;

pub const MACHINE_RELAY_GRANT_VERSION: u16 = 1;
pub const MAX_MACHINE_RELAY_GRANT_BYTES: usize = 2 * 1024;
pub const MAX_MACHINE_RELAY_AUTH_TOKEN_LEN: usize = 4 * MAX_MACHINE_RELAY_GRANT_BYTES.div_ceil(3);
pub const MAX_MACHINE_RELAY_GRANT_LIFETIME_SECS: u64 = 5 * 60;
pub const MAX_MACHINE_RELAY_CLOCK_SKEW_SECS: u64 = 30;
pub const MAX_MACHINE_RELAY_AUDIENCE_LEN: usize = 2_048;
pub const MAX_MACHINE_RELAY_TOKEN_ID_LEN: usize = 128;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MachineRole {
    Scheduler,
    Agent,
    Podctl,
    Proxy,
    Sidecar,
}

impl MachineRole {
    pub fn is_machine_relay_allowed(self) -> bool {
        matches!(self, Self::Scheduler | Self::Agent | Self::Podctl)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineRelayGrant {
    pub version: u16,
    #[serde(with = "serde_bytes")]
    pub subject_endpoint_id: Vec<u8>,
    pub role: MachineRole,
    pub relay_audience: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub token_id: String,
    pub issuer_pubkey: String,
    pub signature: String,
}

impl MachineRelayGrant {
    pub fn sign(
        mut self,
        issuer_public: &[u8],
        issuer_private: &[u8],
        now_secs: u64,
    ) -> Result<Self> {
        self.issuer_pubkey = crypto::b64_encode(issuer_public);
        self.signature.clear();
        self.validate_unsigned(now_secs)?;
        self.signature = crypto::b64_encode(&crypto::sign_data_with_key(
            issuer_private,
            &self.canonical_bytes()?,
        )?);
        self.validate(now_secs)?;
        Ok(self)
    }

    pub fn verify(
        &self,
        trusted_issuers: &[Vec<u8>],
        expected_subject_endpoint_id: &[u8],
        expected_relay_audience: &str,
        now_secs: u64,
    ) -> Result<()> {
        self.validate(now_secs)?;
        ensure!(
            self.subject_endpoint_id == expected_subject_endpoint_id,
            "relay grant subject does not match authenticated endpoint"
        );
        ensure!(
            self.relay_audience == expected_relay_audience,
            "relay grant audience does not match relay"
        );
        ensure!(
            self.role.is_machine_relay_allowed(),
            "role is not allowed on machine relay"
        );
        let issuer_public = crypto::b64_decode(&self.issuer_pubkey)?;
        ensure!(
            trusted_issuers
                .iter()
                .any(|trusted| trusted == &issuer_public),
            "relay grant issuer is not trusted"
        );
        let signature = crypto::b64_decode(&self.signature)?;
        crypto::verify_envelope(&issuer_public, &self.canonical_bytes()?, &signature)
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.validate(now_secs)?;
        let bytes = postcard::to_allocvec(self).context("serialize machine relay grant")?;
        validate_encoded_size(&bytes)?;
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_encoded_size(bytes)?;
        let grant: Self = postcard::from_bytes(bytes).context("decode machine relay grant")?;
        grant.validate(now_secs)?;
        Ok(grant)
    }

    pub fn to_auth_token(&self, now_secs: u64) -> Result<String> {
        Ok(crypto::b64_encode(&self.to_bytes(now_secs)?))
    }

    pub fn from_auth_token(token: &str, now_secs: u64) -> Result<Self> {
        ensure!(
            !token.is_empty() && token.len() <= MAX_MACHINE_RELAY_AUTH_TOKEN_LEN,
            "machine relay authorization token length is invalid"
        );
        Self::from_bytes(&crypto::b64_decode(token)?, now_secs)
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(&Self {
            signature: String::new(),
            ..self.clone()
        })
        .context("serialize canonical machine relay grant")?;
        validate_encoded_size(&bytes)?;
        Ok(bytes)
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        ensure!(
            crypto::b64_decode(&self.issuer_pubkey)?.len() == 32,
            "relay grant issuer key must decode to 32 bytes"
        );
        ensure!(
            crypto::b64_decode(&self.signature)?.len() == 64,
            "relay grant signature must decode to 64 bytes"
        );
        Ok(())
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == MACHINE_RELAY_GRANT_VERSION,
            "unsupported machine relay grant version"
        );
        ensure!(
            self.subject_endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "relay grant subject must contain 32 bytes"
        );
        validate_audience(&self.relay_audience)?;
        ensure!(
            !self.token_id.is_empty() && self.token_id.len() <= MAX_MACHINE_RELAY_TOKEN_ID_LEN,
            "relay grant token ID length is invalid"
        );
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_MACHINE_RELAY_CLOCK_SKEW_SECS),
            "relay grant issue time is too far in the future"
        );
        ensure!(self.expires_at_secs >= now_secs, "relay grant expired");
        ensure!(
            self.expires_at_secs >= self.issued_at_secs,
            "relay grant expiry precedes issue time"
        );
        ensure!(
            self.expires_at_secs.saturating_sub(self.issued_at_secs)
                <= MAX_MACHINE_RELAY_GRANT_LIFETIME_SECS,
            "relay grant lifetime exceeds limit"
        );
        Ok(())
    }
}

fn validate_audience(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_MACHINE_RELAY_AUDIENCE_LEN,
        "relay grant audience length is invalid"
    );
    ensure!(
        value.starts_with("https://") || value.starts_with("http://"),
        "relay grant audience must use HTTP or HTTPS"
    );
    ensure!(
        !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()),
        "relay grant audience contains invalid characters"
    );
    Ok(())
}

fn validate_encoded_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_MACHINE_RELAY_GRANT_BYTES,
        "machine relay grant encoded size is invalid"
    );
    Ok(())
}
