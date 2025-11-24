use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::util::opt_str;

fn serialize<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("protocol serialization should succeed")
}

fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub ok: bool,
    pub status: String,
}

impl Health {
    pub fn status(&self) -> Option<&str> {
        opt_str(&self.status)
    }
}

pub fn build_health(ok: bool, status: &str) -> Vec<u8> {
    serialize(&Health {
        ok,
        status: status.to_string(),
    })
}

pub fn root_as_health(bytes: &[u8]) -> Result<Health, postcard::Error> {
    deserialize(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityRequest {
    pub request_id: String,
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
    pub replicas: u32,
}

impl CapacityRequest {
    pub fn request_id(&self) -> Option<&str> {
        opt_str(&self.request_id)
    }

    pub fn cpu_milli(&self) -> u32 {
        self.cpu_milli
    }

    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    pub fn storage_bytes(&self) -> u64 {
        self.storage_bytes
    }

    pub fn replicas(&self) -> u32 {
        self.replicas
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = request_id.into();
        self
    }
}

impl Default for CapacityRequest {
    fn default() -> Self {
        Self {
            request_id: String::new(),
            cpu_milli: 0,
            memory_bytes: 0,
            storage_bytes: 0,
            replicas: 1,
        }
    }
}

pub fn build_capacity_request(
    cpu_milli: u32,
    memory_bytes: u64,
    storage_bytes: u64,
    replicas: u32,
) -> Vec<u8> {
    build_capacity_request_with_id("", cpu_milli, memory_bytes, storage_bytes, replicas)
}

pub fn build_capacity_request_with_id(
    request_id: &str,
    cpu_milli: u32,
    memory_bytes: u64,
    storage_bytes: u64,
    replicas: u32,
) -> Vec<u8> {
    serialize(&CapacityRequest {
        request_id: request_id.to_string(),
        replicas,
        cpu_milli,
        memory_bytes,
        storage_bytes,
    })
}

pub fn root_as_capacity_request(bytes: &[u8]) -> Result<CapacityRequest, postcard::Error> {
    deserialize(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityReply {
    pub request_id: String,
    pub ok: bool,
    pub node_id: String,
    pub region: String,
    pub kem_pubkey: String,
    pub capabilities: Vec<String>,
    pub cpu_available_milli: u32,
    pub memory_available_bytes: u64,
    pub storage_available_bytes: u64,
}

impl CapacityReply {
    pub fn request_id(&self) -> Option<&str> {
        opt_str(&self.request_id)
    }

    pub fn node_id(&self) -> Option<&str> {
        opt_str(&self.node_id)
    }

    pub fn region(&self) -> Option<&str> {
        opt_str(&self.region)
    }

    pub fn kem_pubkey(&self) -> Option<&str> {
        opt_str(&self.kem_pubkey)
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn cpu_available_milli(&self) -> u32 {
        self.cpu_available_milli
    }

    pub fn memory_available_bytes(&self) -> u64 {
        self.memory_available_bytes
    }

    pub fn storage_available_bytes(&self) -> u64 {
        self.storage_available_bytes
    }
}

pub fn build_capacity_reply(
    ok: bool,
    cpu_available_milli: u32,
    memory_available_bytes: u64,
    storage_available_bytes: u64,
    request_id: &str,
    node_id: &str,
    region: &str,
    kem_pubkey: Option<&str>,
    capabilities: &[&str],
) -> Vec<u8> {
    serialize(&CapacityReply {
        ok,
        cpu_available_milli,
        memory_available_bytes,
        storage_available_bytes,
        request_id: request_id.to_string(),
        node_id: node_id.to_string(),
        region: region.to_string(),
        kem_pubkey: kem_pubkey.unwrap_or_default().to_string(),
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
    })
}

pub fn root_as_capacity_reply(bytes: &[u8]) -> Result<CapacityReply, postcard::Error> {
    deserialize(bytes)
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
}

pub fn build_apply_request(
    replicas: u32,
    operation_id: &str,
    manifest_json: &str,
    origin_peer: &str,
    manifest_id: &str,
) -> Vec<u8> {
    serialize(&ApplyRequest {
        replicas,
        operation_id: operation_id.to_string(),
        manifest_json: manifest_json.to_string(),
        origin_peer: origin_peer.to_string(),
        manifest_id: manifest_id.to_string(),
    })
}

pub fn root_as_apply_request(bytes: &[u8]) -> Result<ApplyRequest, postcard::Error> {
    deserialize(bytes)
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
}

pub fn build_apply_response(ok: bool, operation_id: &str, message: &str) -> Vec<u8> {
    serialize(&ApplyResponse {
        ok,
        operation_id: operation_id.to_string(),
        message: message.to_string(),
    })
}

pub fn root_as_apply_response(bytes: &[u8]) -> Result<ApplyResponse, postcard::Error> {
    deserialize(bytes)
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
}

pub fn build_delete_request(
    manifest_id: &str,
    operation_id: &str,
    origin_peer: &str,
    force: bool,
) -> Vec<u8> {
    serialize(&DeleteRequest {
        manifest_id: manifest_id.to_string(),
        operation_id: operation_id.to_string(),
        origin_peer: origin_peer.to_string(),
        force,
    })
}

pub fn root_as_delete_request(bytes: &[u8]) -> Result<DeleteRequest, postcard::Error> {
    deserialize(bytes)
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
}

pub fn build_delete_response(
    ok: bool,
    operation_id: &str,
    message: &str,
    manifest_id: &str,
    removed_workloads: &[String],
) -> Vec<u8> {
    serialize(&DeleteResponse {
        ok,
        operation_id: operation_id.to_string(),
        message: message.to_string(),
        manifest_id: manifest_id.to_string(),
        removed_workloads: removed_workloads.to_vec(),
    })
}

pub fn root_as_delete_response(bytes: &[u8]) -> Result<DeleteResponse, postcard::Error> {
    deserialize(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    pub nonce: u32,
    pub timestamp: u64,
    pub protocol_version: String,
    pub signature: String,
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
}

pub fn build_handshake(
    nonce: u32,
    timestamp: u64,
    protocol_version: &str,
    signature: &str,
) -> Vec<u8> {
    serialize(&Handshake {
        nonce,
        timestamp,
        protocol_version: protocol_version.to_string(),
        signature: signature.to_string(),
    })
}

pub fn root_as_handshake(bytes: &[u8]) -> Result<Handshake, postcard::Error> {
    deserialize(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodesResponse {
    pub peers: Vec<String>,
}

impl NodesResponse {
    pub fn peers(&self) -> &[String] {
        &self.peers
    }
}

pub fn build_nodes_response(peers: &[String]) -> Vec<u8> {
    serialize(&NodesResponse {
        peers: peers.to_vec(),
    })
}

pub fn root_as_nodes_response(bytes: &[u8]) -> Result<NodesResponse, postcard::Error> {
    deserialize(bytes)
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
    pub fn ok(&self) -> bool {
        self.ok
    }

    pub fn candidates(&self) -> &[CandidateNode] {
        &self.candidates
    }
}

pub fn build_candidates_response_with_keys(ok: bool, candidates: &[(String, String)]) -> Vec<u8> {
    let nodes = candidates
        .iter()
        .map(|(peer_id, public_key)| CandidateNode {
            peer_id: peer_id.clone(),
            public_key: public_key.clone(),
        })
        .collect();
    serialize(&CandidatesResponse {
        ok,
        candidates: nodes,
    })
}

pub fn build_candidates_response(ok: bool, responders: &[String]) -> Vec<u8> {
    let nodes = responders
        .iter()
        .map(|peer_id| CandidateNode {
            peer_id: peer_id.clone(),
            public_key: String::new(),
        })
        .collect::<Vec<_>>();
    serialize(&CandidatesResponse {
        ok,
        candidates: nodes,
    })
}

pub fn root_as_candidates_response(bytes: &[u8]) -> Result<CandidatesResponse, postcard::Error> {
    deserialize(bytes)
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
}

pub fn build_task_create_response(
    ok: bool,
    task_id: &str,
    manifest_id: &str,
    selection_window_ms: u64,
) -> Vec<u8> {
    serialize(&TaskCreateResponse {
        ok,
        task_id: task_id.to_string(),
        manifest_ref: manifest_id.to_string(),
        selection_window_ms,
        message: "task created".to_string(),
    })
}

pub fn root_as_task_create_response(bytes: &[u8]) -> Result<TaskCreateResponse, postcard::Error> {
    deserialize(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStatusResponse {
    pub task_id: String,
    pub state: String,
    pub assigned_peers: Vec<String>,
    pub manifest_cid: String,
}

impl TaskStatusResponse {
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
}

pub fn build_task_status_response(
    task_id: &str,
    state: &str,
    assigned_peers: &[String],
    manifest_cid: Option<&str>,
) -> Vec<u8> {
    serialize(&TaskStatusResponse {
        task_id: task_id.to_string(),
        state: state.to_string(),
        assigned_peers: assigned_peers.to_vec(),
        manifest_cid: manifest_cid.unwrap_or_default().to_string(),
    })
}

pub fn root_as_task_status_response(bytes: &[u8]) -> Result<TaskStatusResponse, postcard::Error> {
    deserialize(bytes)
}

pub fn build_manifest_target(peer_id: &str, payload_json: &str) -> (String, String) {
    (peer_id.to_string(), payload_json.to_string())
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

pub fn compute_manifest_id(name: &str, version: u64) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    version.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn compute_manifest_id_from_content(manifest_data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    manifest_data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
