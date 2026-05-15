use serde::{Deserialize, Serialize};

use super::util::opt_str;

fn ser<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("protocol serialization should succeed")
}

fn de<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

// ─── Phase 0.5: Two-phase capability + resource discovery ──────────────────

fn default_max_hops() -> u8 {
    crate::libp2p_constants::CAPACITY_REQUEST_DEFAULT_MAX_HOPS
}

/// Phase 1 of discovery: gossipsub broadcast asking who can satisfy a trust policy.
/// No resource details are leaked in this message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityQuery {
    /// sha256(trust_policy_bytes + nonce) — opaque identifier for this query round
    pub query_id: String,
    /// Random nonce (hex) to prevent replay
    pub nonce: String,
    /// Required capabilities (e.g. ["gpu", "region:eu"])
    pub required_capabilities: Vec<String>,
    /// Role filter: "worker" | "custodian" | "both" | "" (empty = any)
    pub role_filter: String,
    /// Base64 Ed25519 signing pubkey of the initiator
    pub initiator_pubkey: String,
    /// Number of gossipsub hops remaining
    #[serde(default = "default_max_hops")]
    pub max_hops: u8,
}

impl CapabilityQuery {
    pub fn new(
        query_id: impl Into<String>,
        nonce: impl Into<String>,
        required_capabilities: &[&str],
        role_filter: impl Into<String>,
        initiator_pubkey: impl Into<String>,
    ) -> Self {
        Self {
            query_id: query_id.into(),
            nonce: nonce.into(),
            required_capabilities: required_capabilities.iter().map(|s| s.to_string()).collect(),
            role_filter: role_filter.into(),
            initiator_pubkey: initiator_pubkey.into(),
            max_hops: crate::libp2p_constants::CAPACITY_REQUEST_DEFAULT_MAX_HOPS,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

/// Phase 1 reply: sent directly (request-response) to the initiator when a node
/// decides it matches the CapabilityQuery. Contains the node's identity material
/// but no resource figures.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityReply {
    /// Mirrors CapabilityQuery.query_id
    pub query_id: String,
    /// PeerId string of the responding node
    pub node_id: String,
    /// Base64 X25519 KEM pubkey of this node
    pub kem_pubkey: String,
    /// Postcard-encoded NodeCert (may be empty)
    #[serde(default)]
    pub node_cert_bytes: Vec<u8>,
    /// Advertised capabilities
    pub capabilities: Vec<String>,
    /// Role of this node
    pub role: String,
}

impl CapabilityReply {
    pub fn new(
        query_id: impl Into<String>,
        node_id: impl Into<String>,
        kem_pubkey: impl Into<String>,
        node_cert_bytes: Vec<u8>,
        capabilities: &[&str],
        role: impl Into<String>,
    ) -> Self {
        Self {
            query_id: query_id.into(),
            node_id: node_id.into(),
            kem_pubkey: kem_pubkey.into(),
            node_cert_bytes,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            role: role.into(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

/// Phase 2 of discovery: sent directly to each eligible node (obtained from CapabilityReply)
/// to ask for actual resource availability.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceQuery {
    /// Mirrors CapabilityQuery.query_id for correlation
    pub query_id: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub replicas: u32,
    /// Role filter (same as CapabilityQuery)
    pub role_filter: String,
    /// Base64 signing pubkey of the initiator (for reservation ownership)
    pub owner_pubkey: String,
}

impl ResourceQuery {
    pub fn new(
        query_id: impl Into<String>,
        cpu_milli: u32,
        memory_bytes: u64,
        storage_bytes: u64,
        replicas: u32,
        role_filter: impl Into<String>,
        owner_pubkey: impl Into<String>,
    ) -> Self {
        Self {
            query_id: query_id.into(),
            cpu_milli,
            memory_bytes,
            storage_bytes,
            replicas,
            role_filter: role_filter.into(),
            owner_pubkey: owner_pubkey.into(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

/// Phase 2 reply: node reports its available resources.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceReply {
    /// Mirrors ResourceQuery.query_id
    pub query_id: String,
    pub ok: bool,
    pub node_id: String,
    pub kem_pubkey: String,
    pub available_cpu_milli: u32,
    pub available_memory_bytes: u64,
    pub available_storage_bytes: u64,
    /// Human-readable rejection reason when ok=false
    #[serde(default)]
    pub rejection_reason: String,
    /// Postcard-encoded NodeCert (may be empty)
    #[serde(default)]
    pub node_cert_bytes: Vec<u8>,
}

impl ResourceReply {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        query_id: impl Into<String>,
        ok: bool,
        node_id: impl Into<String>,
        kem_pubkey: impl Into<String>,
        available_cpu_milli: u32,
        available_memory_bytes: u64,
        available_storage_bytes: u64,
        rejection_reason: impl Into<String>,
        node_cert_bytes: Vec<u8>,
    ) -> Self {
        Self {
            query_id: query_id.into(),
            ok,
            node_id: node_id.into(),
            kem_pubkey: kem_pubkey.into(),
            available_cpu_milli,
            available_memory_bytes,
            available_storage_bytes,
            rejection_reason: rejection_reason.into(),
            node_cert_bytes,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub ok: bool,
    pub status: String,
}

impl Health {
    pub fn new(ok: bool, status: impl Into<String>) -> Self {
        Self { ok, status: status.into() }
    }

    pub fn status(&self) -> Option<&str> {
        opt_str(&self.status)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyRequest {
    pub replicas: u32,
    pub operation_id: String,
    pub manifest_json: String,
    pub origin_peer: String,
    pub manifest_id: String,
}

impl ApplyRequest {
    pub fn replicas(&self) -> u32 {
        self.replicas
    }

    pub fn operation_id(&self) -> Option<&str> {
        opt_str(&self.operation_id)
    }

    pub fn manifest_json(&self) -> Option<&str> {
        opt_str(&self.manifest_json)
    }

    pub fn origin_peer(&self) -> Option<&str> {
        opt_str(&self.origin_peer)
    }

    pub fn manifest_id(&self) -> Option<&str> {
        opt_str(&self.manifest_id)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyResponse {
    pub ok: bool,
    pub operation_id: String,
    pub message: String,
}

impl ApplyResponse {
    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn operation_id(&self) -> Option<&str> {
        opt_str(&self.operation_id)
    }

    pub fn message(&self) -> Option<&str> {
        opt_str(&self.message)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteRequest {
    pub manifest_id: String,
    pub operation_id: String,
    pub origin_peer: String,
    pub force: bool,
}

impl DeleteRequest {
    pub fn manifest_id(&self) -> Option<&str> {
        opt_str(&self.manifest_id)
    }

    pub fn operation_id(&self) -> Option<&str> {
        opt_str(&self.operation_id)
    }

    pub fn origin_peer(&self) -> Option<&str> {
        opt_str(&self.origin_peer)
    }

    pub fn force(&self) -> bool {
        self.force
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteResponse {
    pub ok: bool,
    pub operation_id: String,
    pub message: String,
    pub manifest_id: String,
    pub removed_workloads: Vec<String>,
}

impl DeleteResponse {
    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn operation_id(&self) -> Option<&str> {
        opt_str(&self.operation_id)
    }

    pub fn message(&self) -> Option<&str> {
        opt_str(&self.message)
    }

    pub fn manifest_id(&self) -> Option<&str> {
        opt_str(&self.manifest_id)
    }

    pub fn removed_workloads(&self) -> &[String] {
        &self.removed_workloads
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    pub nonce: u32,
    pub timestamp: u64,
    pub protocol_version: String,
    pub signature: String,
    /// Optional base64-postcard `NodeCert` presented by a proxy node so the
    /// peer (typically a sidecar) can verify it shares a tenant. Empty for
    /// non-proxy nodes.
    pub proxy_cert_b64: String,
}

impl Handshake {
    pub fn nonce(&self) -> u32 {
        self.nonce
    }

    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn protocol_version(&self) -> Option<&str> {
        opt_str(&self.protocol_version)
    }

    pub fn signature(&self) -> Option<&str> {
        opt_str(&self.signature)
    }

    pub fn proxy_cert_b64(&self) -> Option<&str> {
        opt_str(&self.proxy_cert_b64)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodesResponse {
    pub peers: Vec<String>,
}

impl NodesResponse {
    pub fn new(peers: &[String]) -> Self {
        Self { peers: peers.to_vec() }
    }

    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateNode {
    pub peer_id: String,
    pub public_key: String,
}

impl CandidateNode {
    pub fn peer_id(&self) -> Option<&str> {
        opt_str(&self.peer_id)
    }

    pub fn public_key(&self) -> Option<&str> {
        opt_str(&self.public_key)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidatesResponse {
    pub ok: bool,
    pub candidates: Vec<CandidateNode>,
}

impl CandidatesResponse {
    pub fn with_keys(ok: bool, candidates: &[(String, String)]) -> Self {
        Self {
            ok,
            candidates: candidates
                .iter()
                .map(|(peer_id, public_key)| CandidateNode {
                    peer_id: peer_id.clone(),
                    public_key: public_key.clone(),
                })
                .collect(),
        }
    }

    pub fn from_peer_ids(ok: bool, responders: &[String]) -> Self {
        Self {
            ok,
            candidates: responders
                .iter()
                .map(|peer_id| CandidateNode {
                    peer_id: peer_id.clone(),
                    public_key: String::new(),
                })
                .collect(),
        }
    }

    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn candidates(&self) -> &[CandidateNode] {
        &self.candidates
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCreateResponse {
    pub ok: bool,
    pub task_id: String,
    pub manifest_ref: String,
    pub selection_window_ms: u64,
    pub message: String,
}

impl TaskCreateResponse {
    pub fn new(ok: bool, task_id: impl Into<String>, manifest_id: impl Into<String>, selection_window_ms: u64) -> Self {
        Self {
            ok,
            task_id: task_id.into(),
            manifest_ref: manifest_id.into(),
            selection_window_ms,
            message: "task created".to_string(),
        }
    }

    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn task_id(&self) -> Option<&str> {
        opt_str(&self.task_id)
    }

    pub fn manifest_ref(&self) -> Option<&str> {
        opt_str(&self.manifest_ref)
    }

    pub fn selection_window_ms(&self) -> u64 {
        self.selection_window_ms
    }

    pub fn message(&self) -> Option<&str> {
        opt_str(&self.message)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStatusResponse {
    pub task_id: String,
    pub state: String,
    pub assigned_peers: Vec<String>,
    pub manifest_cid: String,
}

impl TaskStatusResponse {
    pub fn new(
        task_id: impl Into<String>,
        state: impl Into<String>,
        assigned_peers: &[String],
        manifest_cid: Option<&str>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            state: state.into(),
            assigned_peers: assigned_peers.to_vec(),
            manifest_cid: manifest_cid.unwrap_or_default().to_string(),
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        opt_str(&self.task_id)
    }

    pub fn state(&self) -> Option<&str> {
        opt_str(&self.state)
    }

    pub fn assigned_peers(&self) -> &[String] {
        &self.assigned_peers
    }

    pub fn manifest_cid(&self) -> Option<&str> {
        opt_str(&self.manifest_cid)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        ser(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        de(bytes)
    }
}

pub fn build_manifest_target(peer_id: &str, payload_json: &str) -> (String, String) {
    (peer_id.to_string(), payload_json.to_string())
}

/// Parse a `peer_id:pubkey` entry emitted by the machine when returning candidates.
pub fn parse_peer_with_pubkey(entry: &str) -> Option<(String, String)> {
    let (peer_id, pubkey_b64) = entry.split_once(':')?;
    let peer = peer_id.trim();
    let key = pubkey_b64.trim();
    if peer.is_empty() || key.is_empty() {
        return None;
    }
    Some((peer.to_string(), key.to_string()))
}

pub fn extract_manifest_name(manifest_data: &[u8]) -> Option<String> {
    let manifest_str = std::str::from_utf8(manifest_data).ok()?;
    for line in manifest_str.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn compute_manifest_id(name: &str, version: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(name.as_bytes());
    hasher.update(&version.to_le_bytes());
    let hash = hasher.finalize();
    bytes_to_hex(&hash.as_bytes()[..8])
}

pub fn compute_manifest_id_from_content(manifest_data: &[u8]) -> String {
    let hash = blake3::hash(manifest_data);
    bytes_to_hex(&hash.as_bytes()[..8])
}

// ─── Phase 6: Custodian heartbeat / liveness ───────────────────────────────

/// Gossipsub broadcast by a custodian node every `HEARTBEAT_INTERVAL_SECS` seconds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatPing {
    pub peer_id: String,
    pub timestamp_secs: u64,
    pub custodian_manifest_ids: Vec<String>,
    pub sig: String,
}

impl HeartbeatPing {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = HeartbeatPing { sig: String::new(), ..self.clone() };
        postcard::to_allocvec(&canonical).expect("HeartbeatPing serialization ok")
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("HeartbeatPing serialization ok")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ─── Phase 2.4: Custodian state replication messages ───────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustodianAnnounce {
    pub manifest_id: String,
    pub owner_pubkey: String,
    pub custodian_peer_id: String,
    pub shares_total: u8,
    pub shares_threshold: u8,
    pub share_index: u8,
    pub timestamp_secs: u64,
    pub sig: String,
}

impl CustodianAnnounce {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = CustodianAnnounce { sig: String::new(), ..self.clone() };
        postcard::to_allocvec(&canonical).expect("CustodianAnnounce serialization ok")
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("CustodianAnnounce serialization ok")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustodianWithdraw {
    pub manifest_id: String,
    pub owner_pubkey: String,
    pub custodian_peer_id: String,
    pub timestamp_secs: u64,
    pub sig: String,
}

impl CustodianWithdraw {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = CustodianWithdraw { sig: String::new(), ..self.clone() };
        postcard::to_allocvec(&canonical).expect("CustodianWithdraw serialization ok")
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("CustodianWithdraw serialization ok")
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

/// Compute the gossipsub topic string for custodian announcements for a given owner.
pub fn custodian_topic_for_owner(owner_pubkey_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(owner_pubkey_bytes);
    hasher.update(b"custodians");
    let result = hasher.finalize();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod manifest_id_tests {
    use super::*;
    #[test]
    fn test_manifest_id_is_deterministic_across_runs() {
        let id1 = compute_manifest_id("my-app", 1);
        let id2 = compute_manifest_id("my-app", 1);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }
    #[test]
    fn test_manifest_id_differs_for_different_content() {
        let id1 = compute_manifest_id("app-a", 1);
        let id2 = compute_manifest_id("app-b", 1);
        assert_ne!(id1, id2);
    }
    #[test]
    fn test_manifest_id_from_content_deterministic() {
        let data = b"apiVersion: v1\nkind: Pod";
        let id1 = compute_manifest_id_from_content(data);
        let id2 = compute_manifest_id_from_content(data);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }
    #[test]
    fn test_manifest_id_from_content_differs() {
        let id1 = compute_manifest_id_from_content(b"content-a");
        let id2 = compute_manifest_id_from_content(b"content-b");
        assert_ne!(id1, id2);
    }
}

#[cfg(test)]
mod two_phase_discovery_tests {
    use super::*;

    #[test]
    fn test_capability_query_roundtrip() {
        let q = CapabilityQuery::new("qid-abc", "nonce-123", &["gpu", "region:eu"], "custodian", "base64pubkey");
        let bytes = q.to_bytes();
        let q2 = CapabilityQuery::from_bytes(&bytes).unwrap();
        assert_eq!(q2.query_id, "qid-abc");
        assert_eq!(q2.nonce, "nonce-123");
        assert_eq!(q2.required_capabilities, vec!["gpu", "region:eu"]);
        assert_eq!(q2.role_filter, "custodian");
        assert_eq!(q2.initiator_pubkey, "base64pubkey");
    }

    #[test]
    fn test_capability_reply_roundtrip() {
        let r = CapabilityReply::new("qid-abc", "peer-123", "kem-pub-b64", vec![1, 2, 3], &["gpu"], "worker");
        let bytes = r.to_bytes();
        let r2 = CapabilityReply::from_bytes(&bytes).unwrap();
        assert_eq!(r2.query_id, "qid-abc");
        assert_eq!(r2.node_id, "peer-123");
        assert_eq!(r2.kem_pubkey, "kem-pub-b64");
        assert_eq!(r2.node_cert_bytes, vec![1, 2, 3]);
        assert_eq!(r2.capabilities, vec!["gpu"]);
        assert_eq!(r2.role, "worker");
    }

    #[test]
    fn test_resource_query_roundtrip() {
        let q = ResourceQuery::new("qid-abc", 500, 512 * 1024 * 1024, 10 * 1024 * 1024 * 1024, 2, "worker", "owner-pub-b64");
        let bytes = q.to_bytes();
        let q2 = ResourceQuery::from_bytes(&bytes).unwrap();
        assert_eq!(q2.query_id, "qid-abc");
        assert_eq!(q2.cpu_milli, 500);
        assert_eq!(q2.memory_bytes, 512 * 1024 * 1024);
        assert_eq!(q2.replicas, 2);
        assert_eq!(q2.role_filter, "worker");
        assert_eq!(q2.owner_pubkey, "owner-pub-b64");
    }

    #[test]
    fn test_resource_reply_roundtrip() {
        let r = ResourceReply::new("qid-abc", true, "peer-123", "kem-pub-b64", 800, 1024 * 1024 * 1024, 5 * 1024 * 1024 * 1024, "", vec![]);
        let bytes = r.to_bytes();
        let r2 = ResourceReply::from_bytes(&bytes).unwrap();
        assert_eq!(r2.query_id, "qid-abc");
        assert!(r2.ok);
        assert_eq!(r2.node_id, "peer-123");
        assert_eq!(r2.kem_pubkey, "kem-pub-b64");
        assert_eq!(r2.available_cpu_milli, 800);
    }

    #[test]
    fn test_resource_reply_not_ok() {
        let r = ResourceReply::new("qid-xyz", false, "peer-456", "", 0, 0, 0, "insufficient memory", vec![]);
        let bytes = r.to_bytes();
        let r2 = ResourceReply::from_bytes(&bytes).unwrap();
        assert!(!r2.ok);
        assert_eq!(r2.rejection_reason, "insufficient memory");
    }
}
