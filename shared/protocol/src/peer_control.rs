//! Peer-only typed control envelopes; these are not proxy handshake payloads.
use crate::{
    WorkloadPayload, WorkloadPayloadType,
    service_discovery::{MAX_SERVICE_RECORD_BYTES, ReplicaServiceRecord, validate_service_label},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const PEER_ALPN: &[u8] = b"/podmesh/peer/1";
pub const MAX_PEER_RECORD_BATCH: usize = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PeerOperation {
    Sync {
        cursor: u16,
    },
    Connect {
        manifest_id: String,
        service: String,
        port: u16,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerRequest {
    pub caller: ReplicaServiceRecord,
    pub operation: PeerOperation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PeerResult {
    Records {
        records: Vec<ReplicaServiceRecord>,
        next: Option<u16>,
    },
    Connected,
    Rejected,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerResponse {
    pub responder: ReplicaServiceRecord,
    pub result: PeerResult,
}

impl WorkloadPayload for PeerRequest {
    const TYPE: WorkloadPayloadType = WorkloadPayloadType::PeerRequest;
    fn validate(&self, now: u64) -> Result<()> {
        ensure!(
            self.caller.to_bytes()?.len() <= MAX_SERVICE_RECORD_BYTES,
            "peer caller record exceeds bound"
        );
        self.caller.verify(&self.caller.catalog.owner, now)?;
        match &self.operation {
            PeerOperation::Sync { cursor } => {
                ensure!(*cursor <= 512, "peer sync cursor exceeds bound")
            }
            PeerOperation::Connect {
                manifest_id,
                service,
                port,
            } => {
                ensure!(
                    manifest_id.len() == 64 && manifest_id.is_ascii() && *port != 0,
                    "invalid peer service target"
                );
                validate_service_label(service)?;
            }
        }
        Ok(())
    }
}
impl WorkloadPayload for PeerResponse {
    const TYPE: WorkloadPayloadType = WorkloadPayloadType::PeerResponse;
    fn validate(&self, now: u64) -> Result<()> {
        self.responder.to_bytes()?;
        self.responder.verify(&self.responder.catalog.owner, now)?;
        if let PeerResult::Records { records, next } = &self.result {
            ensure!(
                records.len() <= MAX_PEER_RECORD_BATCH && next.is_none_or(|cursor| cursor <= 512),
                "peer record batch exceeds bound"
            );
            for record in records {
                record.to_bytes()?;
                record.verify(&self.responder.catalog.owner, now)?;
            }
        }
        Ok(())
    }
}
