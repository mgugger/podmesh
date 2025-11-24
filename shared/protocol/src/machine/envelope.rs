use anyhow::Context;
use base64::Engine;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::util::opt_str;

fn serialize<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("envelope serialization should succeed")
}

fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    pub payload_type: String,
    pub nonce: String,
    pub ts: u64,
    pub alg: String,
    pub sig: String,
    pub pubkey: String,
    pub peer_id: String,
    pub kem_pubkey: String,
}

impl Envelope {
    pub fn payload(&self) -> Option<&[u8]> {
        if self.payload.is_empty() {
            None
        } else {
            Some(&self.payload)
        }
    }

    pub fn payload_vec(&self) -> Vec<u8> {
        self.payload.clone()
    }

    pub fn payload_type(&self) -> Option<&str> {
        opt_str(&self.payload_type)
    }

    pub fn nonce(&self) -> Option<&str> {
        opt_str(&self.nonce)
    }

    pub fn ts(&self) -> u64 {
        self.ts
    }

    pub fn alg(&self) -> Option<&str> {
        opt_str(&self.alg)
    }

    pub fn sig(&self) -> Option<&str> {
        opt_str(&self.sig)
    }

    pub fn pubkey(&self) -> Option<&str> {
        opt_str(&self.pubkey)
    }

    pub fn peer_id(&self) -> Option<&str> {
        opt_str(&self.peer_id)
    }

    pub fn kem_pubkey(&self) -> Option<&str> {
        opt_str(&self.kem_pubkey)
    }
}

fn base_envelope(
    payload: &[u8],
    payload_type: &str,
    nonce: &str,
    ts: u64,
    alg: &str,
    peer_id: &str,
    sig: &str,
    pubkey: &str,
    kem_pubkey: Option<&str>,
) -> Envelope {
    Envelope {
        payload: payload.to_vec(),
        payload_type: payload_type.to_string(),
        nonce: nonce.to_string(),
        ts,
        alg: alg.to_string(),
        sig: sig.to_string(),
        pubkey: pubkey.to_string(),
        peer_id: peer_id.to_string(),
        kem_pubkey: kem_pubkey.unwrap_or_default().to_string(),
    }
}

pub fn build_envelope_canonical(
    payload: &[u8],
    payload_type: &str,
    nonce: &str,
    ts: u64,
    alg: &str,
    kem_pub: Option<&str>,
) -> Vec<u8> {
    serialize(&base_envelope(
        payload,
        payload_type,
        nonce,
        ts,
        alg,
        "",
        "",
        "",
        kem_pub,
    ))
}

pub fn build_envelope_signed(
    payload: &[u8],
    payload_type: &str,
    nonce: &str,
    ts: u64,
    alg: &str,
    sig_prefix: &str,
    sig_b64: &str,
    pubkey_b64: &str,
    kem_pub_b64: Option<&str>,
) -> Vec<u8> {
    let sig_full = format!("{}:{}", sig_prefix, sig_b64);
    serialize(&base_envelope(
        payload,
        payload_type,
        nonce,
        ts,
        alg,
        "",
        &sig_full,
        pubkey_b64,
        kem_pub_b64,
    ))
}

pub fn build_envelope_canonical_with_peer(
    payload: &[u8],
    payload_type: &str,
    nonce: &str,
    timestamp: u64,
    algorithm: &str,
    peer_id: &str,
    kem_pub: Option<&str>,
) -> Vec<u8> {
    serialize(&base_envelope(
        payload,
        payload_type,
        nonce,
        timestamp,
        algorithm,
        peer_id,
        "",
        "",
        kem_pub,
    ))
}

pub fn build_envelope_signed_with_peer(
    payload: &[u8],
    payload_type: &str,
    nonce: &str,
    timestamp: u64,
    algorithm: &str,
    sig_prefix: &str,
    sig_b64: &str,
    pubkey_b64: &str,
    peer_id: &str,
    kem_pub_b64: Option<&str>,
) -> Vec<u8> {
    let sig_full = format!("{}:{}", sig_prefix, sig_b64);
    serialize(&base_envelope(
        payload,
        payload_type,
        nonce,
        timestamp,
        algorithm,
        peer_id,
        &sig_full,
        pubkey_b64,
        kem_pub_b64,
    ))
}

pub fn root_as_envelope(bytes: &[u8]) -> Result<Envelope, postcard::Error> {
    deserialize(bytes)
}

pub fn fb_envelope_extract_sig_pub(envelope_bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let envelope = root_as_envelope(envelope_bytes).ok()?;
    let sig_field = envelope.sig()?;
    let sig_b64 = sig_field
        .splitn(2, ':')
        .nth(if sig_field.contains(':') { 1 } else { 0 })
        .unwrap_or(sig_field);
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .ok()?;
    let pub_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.pubkey()?)
        .ok()?;
    Some((sig_bytes, pub_bytes))
}

pub fn fb_envelope_extract_sig_pub_legacy(
    buf: &[u8],
) -> anyhow::Result<(Vec<u8>, Vec<u8>, Vec<u8>, String, String)> {
    let env =
        root_as_envelope(buf).map_err(|e| anyhow::anyhow!("failed to parse envelope: {e}"))?;

    let canonical = build_envelope_canonical(
        env.payload().unwrap_or(&[]),
        env.payload_type().unwrap_or(""),
        env.nonce().unwrap_or(""),
        env.ts(),
        env.alg().unwrap_or(""),
        env.kem_pubkey(),
    );

    let sig_field = env.sig().unwrap_or("").to_string();
    let pubkey_field = env.pubkey().unwrap_or("").to_string();

    let sig_b64 = sig_field
        .splitn(2, ':')
        .nth(if sig_field.contains(':') { 1 } else { 0 })
        .unwrap_or(&sig_field)
        .to_string();
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&sig_b64)
        .context("failed to base64-decode signature")?;
    let pub_bytes = base64::engine::general_purpose::STANDARD
        .decode(&pubkey_field)
        .context("failed to base64-decode pubkey")?;

    Ok((canonical, sig_bytes, pub_bytes, sig_field, pubkey_field))
}

pub fn build_encrypted_envelope(
    payload: &[u8],
    payload_type: &str,
    recipient_pubkey: &[u8],
    sender_privkey: &[u8],
    sender_pubkey: &str,
) -> anyhow::Result<Vec<u8>> {
    let encrypted_payload = crypto::encrypt_payload_for_recipient(recipient_pubkey, payload)?;

    let (nonce, ts) = envelope_nonce_and_timestamp();

    let canonical = build_envelope_canonical(
        &encrypted_payload,
        payload_type,
        &nonce,
        ts,
        "ml-dsa-65",
        None,
    );

    let sender_pubkey_bytes = base64::engine::general_purpose::STANDARD
        .decode(sender_pubkey)
        .context("failed to decode sender public key")?;
    let (sig_b64, pub_b64) =
        crypto::sign_envelope(sender_privkey, &sender_pubkey_bytes, &canonical)?;

    Ok(build_envelope_signed(
        &encrypted_payload,
        payload_type,
        &nonce,
        ts,
        "ml-dsa-65",
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        None,
    ))
}

pub fn build_encrypted_envelope_with_peer(
    payload: &[u8],
    payload_type: &str,
    recipient_pubkey: &[u8],
    sender_privkey: &[u8],
    sender_pubkey: &str,
    peer_id: &str,
    sender_kem_pub_b64: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    let encrypted_payload = crypto::encrypt_payload_for_recipient(recipient_pubkey, payload)?;

    let (nonce, ts) = envelope_nonce_and_timestamp();

    let canonical = build_envelope_canonical_with_peer(
        &encrypted_payload,
        payload_type,
        &nonce,
        ts,
        "ml-dsa-65",
        peer_id,
        sender_kem_pub_b64,
    );

    let sender_pubkey_bytes = base64::engine::general_purpose::STANDARD
        .decode(sender_pubkey)
        .context("failed to decode sender public key")?;
    let (sig_b64, pub_b64) =
        crypto::sign_envelope(sender_privkey, &sender_pubkey_bytes, &canonical)?;

    Ok(build_envelope_signed_with_peer(
        &encrypted_payload,
        payload_type,
        &nonce,
        ts,
        "ml-dsa-65",
        "ml-dsa-65",
        &sig_b64,
        &pub_b64,
        peer_id,
        sender_kem_pub_b64,
    ))
}

fn envelope_nonce_and_timestamp() -> (String, u64) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    let nonce = format!("{:x}", hasher.finish());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (nonce, ts)
}
