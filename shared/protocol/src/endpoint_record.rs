use std::{collections::HashSet, net::SocketAddr};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const ENDPOINT_RECORD_VERSION: u16 = 1;
pub const IROH_ENDPOINT_ID_BYTES: usize = 32;
pub const MAX_ENDPOINT_DIRECT_ADDRESSES: usize = 8;
pub const MAX_ENDPOINT_DIRECT_ADDRESS_LEN: usize = 128;
pub const MAX_ENDPOINT_RELAY_URL_LEN: usize = 2_048;
pub const MAX_ENDPOINT_RECORD_BYTES: usize = 4 * 1024;
pub const MAX_ENDPOINT_RECORD_LIFETIME_SECS: u64 = 60 * 60;
pub const MAX_ENDPOINT_RECORD_CLOCK_SKEW_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointRecord {
    pub version: u16,
    #[serde(with = "serde_bytes")]
    pub endpoint_id: Vec<u8>,
    pub relay_url: Option<String>,
    pub direct_addresses: Vec<String>,
    pub signing_pubkey: String,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub signature: String,
}

impl EndpointRecord {
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
        let signing_public = crypto::b64_decode(&self.signing_pubkey)?;
        let signature = crypto::b64_decode(&self.signature)?;
        crypto::verify_envelope(&signing_public, &self.canonical_bytes()?, &signature)
    }

    pub fn to_bytes(&self, now_secs: u64) -> Result<Vec<u8>> {
        self.verify(now_secs)?;
        let bytes = postcard::to_allocvec(self).context("serialize endpoint record")?;
        validate_encoded_size(&bytes)?;
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8], now_secs: u64) -> Result<Self> {
        validate_encoded_size(bytes)?;
        let record: Self = postcard::from_bytes(bytes).context("decode endpoint record")?;
        record.verify(now_secs)?;
        Ok(record)
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(&Self {
            signature: String::new(),
            ..self.clone()
        })
        .context("serialize canonical endpoint record")?;
        validate_encoded_size(&bytes)?;
        Ok(bytes)
    }

    fn validate(&self, now_secs: u64) -> Result<()> {
        self.validate_unsigned(now_secs)?;
        let signing_public = crypto::b64_decode(&self.signing_pubkey)?;
        ensure!(
            signing_public.len() == 32,
            "signing public key must decode to 32 bytes"
        );
        let signature = crypto::b64_decode(&self.signature)?;
        ensure!(signature.len() == 64, "signature must decode to 64 bytes");
        Ok(())
    }

    fn validate_unsigned(&self, now_secs: u64) -> Result<()> {
        ensure!(
            self.version == ENDPOINT_RECORD_VERSION,
            "unsupported endpoint record version"
        );
        ensure!(
            self.endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "endpoint ID must contain 32 bytes"
        );
        ensure!(
            self.relay_url.is_some() || !self.direct_addresses.is_empty(),
            "endpoint record must contain a relay or direct address"
        );
        if let Some(relay_url) = &self.relay_url {
            validate_relay_url(relay_url)?;
        }
        validate_direct_addresses(&self.direct_addresses)?;
        ensure!(
            self.issued_at_secs <= now_secs.saturating_add(MAX_ENDPOINT_RECORD_CLOCK_SKEW_SECS),
            "endpoint record issue time is too far in the future"
        );
        ensure!(self.expires_at_secs >= now_secs, "endpoint record expired");
        ensure!(
            self.expires_at_secs >= self.issued_at_secs,
            "endpoint record expiry precedes issue time"
        );
        ensure!(
            self.expires_at_secs.saturating_sub(self.issued_at_secs)
                <= MAX_ENDPOINT_RECORD_LIFETIME_SECS,
            "endpoint record lifetime exceeds limit"
        );
        Ok(())
    }
}

fn validate_relay_url(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_ENDPOINT_RELAY_URL_LEN,
        "relay URL length is invalid"
    );
    ensure!(
        value.starts_with("https://") || value.starts_with("http://"),
        "relay URL must use HTTP or HTTPS"
    );
    ensure!(
        !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()),
        "relay URL contains invalid characters"
    );
    Ok(())
}

fn validate_direct_addresses(addresses: &[String]) -> Result<()> {
    ensure!(
        addresses.len() <= MAX_ENDPOINT_DIRECT_ADDRESSES,
        "too many direct endpoint addresses"
    );
    let mut unique = HashSet::with_capacity(addresses.len());
    for address in addresses {
        ensure!(
            !address.is_empty() && address.len() <= MAX_ENDPOINT_DIRECT_ADDRESS_LEN,
            "direct endpoint address length is invalid"
        );
        let parsed: SocketAddr = address.parse().context("invalid direct endpoint address")?;
        if !unique.insert(parsed) {
            bail!("duplicate direct endpoint address");
        }
    }
    Ok(())
}

fn validate_encoded_size(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_ENDPOINT_RECORD_BYTES,
        "endpoint record encoded size is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000;

    fn signed_record() -> EndpointRecord {
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
        EndpointRecord {
            version: ENDPOINT_RECORD_VERSION,
            endpoint_id: vec![7; IROH_ENDPOINT_ID_BYTES],
            relay_url: Some("https://relay.example.test".into()),
            direct_addresses: vec!["192.0.2.10:4000".into(), "[2001:db8::1]:4000".into()],
            signing_pubkey: String::new(),
            issued_at_secs: NOW,
            expires_at_secs: NOW + 300,
            signature: String::new(),
        }
        .sign(&public, &private, NOW)
        .unwrap()
    }

    #[test]
    fn signed_record_roundtrips() {
        let record = signed_record();
        let decoded = EndpointRecord::from_bytes(&record.to_bytes(NOW).unwrap(), NOW).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn signature_binds_endpoint_identity() {
        let mut record = signed_record();
        record.endpoint_id[0] ^= 1;
        assert!(record.verify(NOW).is_err());
    }

    #[test]
    fn rejects_invalid_endpoint_identity_length() {
        let mut record = signed_record();
        record.endpoint_id.pop();
        assert!(record.verify(NOW).is_err());
    }

    #[test]
    fn rejects_duplicate_and_invalid_direct_addresses() {
        let mut duplicate = signed_record();
        duplicate.direct_addresses.push("192.0.2.10:4000".into());
        assert!(duplicate.verify(NOW).is_err());

        let mut malformed = signed_record();
        malformed.direct_addresses = vec!["not-a-socket".into()];
        assert!(malformed.verify(NOW).is_err());
    }

    #[test]
    fn rejects_unbounded_addresses_and_lifetime() {
        let mut record = signed_record();
        record.direct_addresses = vec!["192.0.2.10:4000".into(); MAX_ENDPOINT_DIRECT_ADDRESSES + 1];
        assert!(record.verify(NOW).is_err());

        let mut record = signed_record();
        record.expires_at_secs = NOW + MAX_ENDPOINT_RECORD_LIFETIME_SECS + 1;
        assert!(record.verify(NOW).is_err());
    }

    #[test]
    fn rejects_expired_or_unreachable_records() {
        let mut expired = signed_record();
        expired.expires_at_secs = NOW - 1;
        assert!(expired.verify(NOW).is_err());

        let mut unreachable = signed_record();
        unreachable.relay_url = None;
        unreachable.direct_addresses.clear();
        assert!(unreachable.verify(NOW).is_err());
    }
}
