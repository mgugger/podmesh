use serde::{Deserialize, Serialize};
use crypto::{b64_decode, b64_encode, sign_envelope, verify_envelope};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeRole {
    Worker,
    Custodian,
    Both,
}

impl Default for NodeRole {
    fn default() -> Self { NodeRole::Both }
}

impl std::fmt::Display for NodeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeRole::Worker => write!(f, "worker"),
            NodeRole::Custodian => write!(f, "custodian"),
            NodeRole::Both => write!(f, "both"),
        }
    }
}

impl std::str::FromStr for NodeRole {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "worker" => Ok(NodeRole::Worker),
            "custodian" => Ok(NodeRole::Custodian),
            "both" => Ok(NodeRole::Both),
            _ => Err(anyhow::anyhow!("unknown role: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Endorsement {
    pub endorser_peer_id: String,
    pub endorser_sig: String,  // base64 Ed25519 sig over NodeCert canonical bytes
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeCert {
    pub peer_id: String,
    pub kem_pubkey: String,        // base64 X25519
    pub signing_pubkey: String,    // base64 Ed25519
    pub capabilities: Vec<String>, // ["gpu", "region:eu", "custodian"]
    pub role: NodeRole,
    pub valid_until: u64,          // unix timestamp seconds
    pub owner_pubkey: String,      // base64 Ed25519 — who issued this cert
    pub owner_sig: String,         // base64 Ed25519 sig over canonical bytes
    pub endorsements: Vec<Endorsement>,
}

impl NodeCert {
    /// Canonical bytes = postcard serialization of cert with owner_sig="" and endorsements=[]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = NodeCert {
            owner_sig: String::new(),
            endorsements: vec![],
            ..self.clone()
        };
        postcard::to_allocvec(&canonical).expect("NodeCert serialization should succeed")
    }

    /// Sign this cert with the owner's Ed25519 signing key.
    /// owner_sk_bytes: raw 32-byte Ed25519 private key
    /// owner_pk_bytes: raw 32-byte Ed25519 public key
    pub fn sign(mut self, owner_sk_bytes: &[u8], owner_pk_bytes: &[u8]) -> anyhow::Result<Self> {
        let canonical = self.canonical_bytes();
        let (sig_b64, _pub_b64) = sign_envelope(owner_sk_bytes, owner_pk_bytes, &canonical)?;
        self.owner_sig = sig_b64;
        Ok(self)
    }

    /// Verify the owner's signature on this cert.
    pub fn verify(&self) -> anyhow::Result<()> {
        let canonical = self.canonical_bytes();
        let owner_pk = b64_decode(&self.owner_pubkey)?;
        let owner_sig = b64_decode(&self.owner_sig)?;
        verify_envelope(&owner_pk, &canonical, &owner_sig)?;
        Ok(())
    }

    /// Returns true if the cert has expired.
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.valid_until < now
    }

    /// Returns true if the cert satisfies a role requirement.
    pub fn has_role(&self, role: &NodeRole) -> bool {
        match role {
            NodeRole::Worker => matches!(self.role, NodeRole::Worker | NodeRole::Both),
            NodeRole::Custodian => matches!(self.role, NodeRole::Custodian | NodeRole::Both),
            NodeRole::Both => matches!(self.role, NodeRole::Both),
        }
    }

    /// Serialize to bytes (postcard)
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("NodeCert serialization should succeed")
    }

    /// Deserialize from bytes (postcard)
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }

    /// Base64-encode the cert bytes for transport
    pub fn to_b64(&self) -> String {
        b64_encode(&self.to_bytes())
    }

    /// Decode from base64
    pub fn from_b64(s: &str) -> anyhow::Result<Self> {
        let bytes = b64_decode(s)?;
        Ok(Self::from_bytes(&bytes)?)
    }
}

/// Default path for node cert on disk
pub fn default_node_cert_path(key_dir: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(key_dir).join("node_cert.bin")
}

/// Load NodeCert from disk. Returns None if the file doesn't exist.
pub fn load_node_cert(key_dir: &str) -> anyhow::Result<Option<NodeCert>> {
    let path = default_node_cert_path(key_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    Ok(Some(NodeCert::from_bytes(&bytes)?))
}

/// Save NodeCert to disk.
pub fn save_node_cert(key_dir: &str, cert: &NodeCert) -> anyhow::Result<()> {
    let path = default_node_cert_path(key_dir);
    std::fs::write(&path, cert.to_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::ensure_keypair_ephemeral;

    fn make_test_cert(role: NodeRole) -> (NodeCert, Vec<u8>, Vec<u8>) {
        let (pk, sk) = ensure_keypair_ephemeral().unwrap();
        let (kem_pk, _kem_sk) = ensure_keypair_ephemeral().unwrap();
        let cert = NodeCert {
            peer_id: "QmTestPeer".to_string(),
            kem_pubkey: b64_encode(&kem_pk),
            signing_pubkey: b64_encode(&pk),
            capabilities: vec!["gpu".to_string(), "region:eu".to_string()],
            role,
            valid_until: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() + 86400,
            owner_pubkey: b64_encode(&pk),
            owner_sig: String::new(),
            endorsements: vec![],
        };
        (cert, sk, pk)
    }

    #[test]
    fn test_node_cert_sign_verify_roundtrip() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Both);
        let signed = cert.sign(&sk, &pk).unwrap();
        assert!(signed.verify().is_ok());
    }

    #[test]
    fn test_node_cert_rejects_tampered_capabilities() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Worker);
        let mut signed = cert.sign(&sk, &pk).unwrap();
        signed.capabilities.push("extra".to_string());
        assert!(signed.verify().is_err());
    }

    #[test]
    fn test_node_cert_is_expired() {
        let (mut cert, sk, pk) = make_test_cert(NodeRole::Worker);
        cert.valid_until = 1; // far in the past
        let signed = cert.sign(&sk, &pk).unwrap();
        assert!(signed.is_expired());
    }

    #[test]
    fn test_node_cert_not_expired() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Worker);
        let signed = cert.sign(&sk, &pk).unwrap();
        assert!(!signed.is_expired());
    }

    #[test]
    fn test_node_cert_role_serialization() {
        for role in [NodeRole::Worker, NodeRole::Custodian, NodeRole::Both] {
            let s = role.to_string();
            let parsed: NodeRole = s.parse().unwrap();
            assert_eq!(parsed.to_string(), s);
        }
    }

    #[test]
    fn test_node_cert_has_role() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Both);
        let signed = cert.sign(&sk, &pk).unwrap();
        assert!(signed.has_role(&NodeRole::Worker));
        assert!(signed.has_role(&NodeRole::Custodian));
        assert!(signed.has_role(&NodeRole::Both));
    }

    #[test]
    fn test_node_cert_worker_role_not_custodian() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Worker);
        let signed = cert.sign(&sk, &pk).unwrap();
        assert!(signed.has_role(&NodeRole::Worker));
        assert!(!signed.has_role(&NodeRole::Custodian));
    }

    #[test]
    fn test_node_cert_bytes_roundtrip() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Custodian);
        let signed = cert.sign(&sk, &pk).unwrap();
        let bytes = signed.to_bytes();
        let recovered = NodeCert::from_bytes(&bytes).unwrap();
        assert_eq!(signed, recovered);
    }

    #[test]
    fn test_node_cert_b64_roundtrip() {
        let (cert, sk, pk) = make_test_cert(NodeRole::Both);
        let signed = cert.sign(&sk, &pk).unwrap();
        let b64 = signed.to_b64();
        let recovered = NodeCert::from_b64(&b64).unwrap();
        assert_eq!(signed, recovered);
    }
}
