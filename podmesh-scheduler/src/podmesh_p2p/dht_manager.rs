#![allow(dead_code)]
use libp2p::{kad, Swarm};
use log::{info, warn};
use protocol::machine::{
    build_applied_manifest, root_as_applied_manifest, AppliedManifest, KeyValue, OperationType,
    SignatureScheme,
};
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::podmesh_p2p::behaviour::MyBehaviour;

/// DHT operations for managing AppliedManifest records
pub enum DhtOperation {
    /// Store an AppliedManifest in the DHT
    StoreManifest {
        manifest: AppliedManifest,
        reply_tx: StoreReplyTx,
    },
    /// Retrieve an AppliedManifest by its ID
    GetManifest {
        id: String,
        reply_tx: GetReplyTx,
    },
    /// Get all manifests by a specific peer
    GetManifestsByPeer {
        peer_id: String,
        reply_tx: mpsc::UnboundedSender<Result<Vec<AppliedManifest>, String>>,
    },
}

pub struct DhtManager {
    /// Pending DHT queries awaiting responses
    pending_queries: HashMap<kad::QueryId, DhtQueryContext>,
}

enum DhtQueryContext {
    StoreManifest {
        reply_tx: StoreReplyTx,
    },
    GetManifest {
        reply_tx: GetReplyTx,
    },
}

type StoreReplyTx = mpsc::UnboundedSender<Result<(), String>>;
type GetReplyTx = mpsc::UnboundedSender<Result<Option<AppliedManifest>, String>>;

impl DhtManager {
    pub fn new() -> Self {
        Self {
            pending_queries: HashMap::new(),
        }
    }

    /// Generate a DHT key for storing/retrieving manifests by ID
    pub fn manifest_key(id: &str) -> kad::RecordKey {
        kad::RecordKey::new(&format!("manifest:{}", id))
    }

    /// Generate a DHT key for a given manifest ID
    /// Generate a DHT key for peer indexing
    pub fn peer_index_key(peer_id: &str) -> kad::RecordKey {
        kad::RecordKey::new(&format!("peer-index:{}", peer_id))
    }

    /// Handle a DHT operation request
    pub fn handle_operation(&mut self, operation: DhtOperation, swarm: &mut Swarm<MyBehaviour>) {
        match operation {
            DhtOperation::StoreManifest { manifest, reply_tx } => {
                self.store_manifest(manifest, reply_tx, swarm);
            }
            DhtOperation::GetManifest { id, reply_tx } => {
                self.get_manifest(id, reply_tx, swarm);
            }
            DhtOperation::GetManifestsByPeer {
                peer_id: _,
                reply_tx,
            } => {
                // For now, send an error as this requires more complex indexing
                let _ = reply_tx.send(Err("Peer queries not implemented yet".to_string()));
            }
        }
    }

    fn store_manifest(
        &mut self,
        manifest: AppliedManifest,
        reply_tx: StoreReplyTx,
        swarm: &mut Swarm<MyBehaviour>,
    ) {
        // Extract the manifest ID
        let manifest_id = match manifest.id() {
            Some(id) => id,
            None => {
                Self::send_err(reply_tx, "Manifest missing ID");
                return;
            }
        };

        let manifest_bytes = manifest.clone().serialize_vec();

        let record_key = Self::manifest_key(&manifest_id);
        let record = kad::Record {
            key: record_key,
            value: manifest_bytes,
            publisher: None,
            expires: None,
        };

        match swarm
            .behaviour_mut()
            .kademlia
            .put_record(record, kad::Quorum::One)
        {
            Ok(query_id) => {
                info!(
                    "DHT: Initiated store operation for manifest {} (query_id: {:?})",
                    manifest_id, query_id
                );
                self.pending_queries
                    .insert(query_id, DhtQueryContext::StoreManifest { reply_tx });
            }
            Err(e) => {
                Self::send_err(reply_tx, format!("Failed to initiate DHT store: {:?}", e));
            }
        }
    }

    fn get_manifest(
        &mut self,
        id: String,
        reply_tx: GetReplyTx,
        swarm: &mut Swarm<MyBehaviour>,
    ) {
        let record_key = Self::manifest_key(&id);
        let query_id = swarm.behaviour_mut().kademlia.get_record(record_key);

        info!(
            "DHT: Initiated get operation for manifest {} (query_id: {:?})",
            id, query_id
        );

        self.pending_queries
            .insert(query_id, DhtQueryContext::GetManifest { reply_tx });
    }

    /// Handle Kademlia query results
    pub fn handle_query_result(&mut self, query_id: kad::QueryId, result: kad::QueryResult) {
        if let Some(context) = self.pending_queries.remove(&query_id) {
            match context {
                DhtQueryContext::StoreManifest { reply_tx } => {
                    Self::handle_store_query_result(reply_tx, result);
                }
                DhtQueryContext::GetManifest { reply_tx } => {
                    Self::handle_get_query_result(reply_tx, result);
                }
            }
        }
    }

    fn handle_store_query_result(reply_tx: StoreReplyTx, result: kad::QueryResult) {
        match result {
            kad::QueryResult::PutRecord(Ok(_)) => Self::send_reply(reply_tx, Ok(())),
            kad::QueryResult::PutRecord(Err(e)) => {
                Self::send_err(reply_tx, format!("Failed to store manifest: {:?}", e));
            }
            other => {
                Self::send_err(reply_tx, format!("Unexpected query result: {:?}", other));
            }
        }
    }

    fn handle_get_query_result(reply_tx: GetReplyTx, result: kad::QueryResult) {
        match result {
            kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(peer_record))) => {
                match root_as_applied_manifest(&peer_record.record.value) {
                    Ok(manifest) => Self::send_reply(reply_tx, Ok(Some(manifest))),
                    Err(e) => Self::send_err(reply_tx, format!("Failed to parse manifest: {:?}", e)),
                }
            }
            kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. })) => {
                Self::send_reply(reply_tx, Ok(None));
            }
            kad::QueryResult::GetRecord(Err(e)) => {
                Self::send_err(reply_tx, format!("Failed to get manifest: {:?}", e));
            }
            other => {
                Self::send_err(reply_tx, format!("Unexpected query result: {:?}", other));
            }
        }
    }

    fn send_reply<T>(tx: mpsc::UnboundedSender<Result<T, String>>, result: Result<T, String>) {
        if tx.send(result).is_err() {
            warn!("Dropping DHT reply because receiver disappeared");
        }
    }

    fn send_err<T>(tx: mpsc::UnboundedSender<Result<T, String>>, message: impl Into<String>) {
        Self::send_reply(tx, Err(message.into()));
    }
}

/// Helper function to create an AppliedManifest for a deployed workload
pub fn create_applied_manifest_for_deployment(
    id: String,
    operation_id: String,
    origin_peer: String,
    manifest_json: String,
    manifest_kind: String,
) -> Vec<u8> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let content_hash = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        manifest_json.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    };

    let kind_label_value = manifest_kind.clone();

    let manifest = AppliedManifest {
        id,
        operation_id,
        origin_peer,
        owner_pubkey: Vec::new(),
        signature_scheme: SignatureScheme::None,
        signature: Vec::new(),
        manifest_json,
        manifest_kind,
        labels: vec![
            KeyValue {
                key: "deployed-by".into(),
                value: "podmesh-node".into(),
            },
            KeyValue {
                key: "kind".into(),
                value: kind_label_value,
            },
        ],
        timestamp,
        operation: OperationType::Apply,
        ttl_secs: 3600,
        content_hash,
    };

    build_applied_manifest(manifest)
}
