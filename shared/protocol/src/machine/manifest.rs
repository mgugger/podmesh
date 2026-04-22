use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::util::opt_str;

fn serialize<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("manifest serialization should succeed")
}

fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum SignatureScheme {
    None = 0,
    Ed25519 = 1,
    RsaPss = 2,
}

impl Default for SignatureScheme {
    fn default() -> Self {
        SignatureScheme::None
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum OperationType {
    Apply = 0,
    Update = 1,
    Delete = 2,
}

impl Default for OperationType {
    fn default() -> Self {
        OperationType::Apply
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

impl KeyValue {
    pub fn key(&self) -> Option<&str> {
        opt_str(&self.key)
    }

    pub fn value(&self) -> Option<&str> {
        opt_str(&self.value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedManifest {
    pub id: String,
    pub operation_id: String,
    pub origin_peer: String,
    #[serde(with = "serde_bytes")]
    pub owner_pubkey: Vec<u8>,
    pub signature_scheme: SignatureScheme,
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
    pub manifest_json: String,
    pub manifest_kind: String,
    pub labels: Vec<KeyValue>,
    pub timestamp: u64,
    pub operation: OperationType,
    pub ttl_secs: u32,
    pub content_hash: String,
}

impl AppliedManifest {
    pub fn id(&self) -> Option<&str> {
        opt_str(&self.id)
    }

    pub fn operation_id(&self) -> Option<&str> {
        opt_str(&self.operation_id)
    }

    pub fn origin_peer(&self) -> Option<&str> {
        opt_str(&self.origin_peer)
    }

    pub fn owner_pubkey(&self) -> Option<&[u8]> {
        if self.owner_pubkey.is_empty() {
            None
        } else {
            Some(&self.owner_pubkey)
        }
    }

    pub fn signature_scheme(&self) -> SignatureScheme {
        self.signature_scheme
    }

    pub fn signature(&self) -> Option<&[u8]> {
        if self.signature.is_empty() {
            None
        } else {
            Some(&self.signature)
        }
    }

    pub fn manifest_json(&self) -> Option<&str> {
        opt_str(&self.manifest_json)
    }

    pub fn manifest_kind(&self) -> Option<&str> {
        opt_str(&self.manifest_kind)
    }

    pub fn labels(&self) -> Option<&[KeyValue]> {
        if self.labels.is_empty() {
            None
        } else {
            Some(&self.labels)
        }
    }

    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn operation(&self) -> OperationType {
        self.operation
    }

    pub fn ttl_secs(&self) -> u32 {
        self.ttl_secs
    }

    pub fn content_hash(&self) -> Option<&str> {
        opt_str(&self.content_hash)
    }
}

pub fn build_applied_manifest(manifest: AppliedManifest) -> Vec<u8> {
    serialize(&manifest)
}

pub fn root_as_applied_manifest(bytes: &[u8]) -> Result<AppliedManifest, postcard::Error> {
    deserialize(bytes)
}

impl AppliedManifest {
    pub fn serialize_vec(self) -> Vec<u8> {
        build_applied_manifest(self)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serialize(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        deserialize(bytes)
    }
}
