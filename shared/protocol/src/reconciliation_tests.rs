use super::reconciliation::*;
use crate::{EndpointRecord, IROH_ENDPOINT_ID_BYTES, WorkloadListRequest};

const NOW: u64 = 1_700_000_000;

fn endpoint(public: &[u8], private: &[u8], id: u8) -> EndpointRecord {
    EndpointRecord {
        version: crate::ENDPOINT_RECORD_VERSION,
        endpoint_id: vec![id; IROH_ENDPOINT_ID_BYTES],
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4000".into()],
        signing_pubkey: String::new(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 60,
        signature: String::new(),
    }
    .sign(public, private, NOW)
    .unwrap()
}

fn owner_request() -> Vec<u8> {
    let (public, private) = crypto::generate_signing_keypair();
    WorkloadListRequest {
        version: crate::AGENT_PROTOCOL_VERSION,
        request_id: "list-1".into(),
        namespace_id: crypto::b64_encode(&public),
        response_kem_pubkey: crypto::b64_encode(&[8; 32]),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 60,
        nonce: "nonce-1".into(),
        owner_signature: String::new(),
    }
    .sign(&private)
    .unwrap()
    .to_bytes()
    .unwrap()
}

fn query(public: &[u8], private: &[u8]) -> SchedulerReconciliationQuery {
    SchedulerReconciliationQuery {
        version: RECONCILIATION_PROTOCOL_VERSION,
        query_id: "reconcile-1".into(),
        owner_request: owner_request(),
        reply_endpoint: endpoint(public, private, 7),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(public, private, NOW)
    .unwrap()
}

#[test]
fn signed_query_round_trips_through_gossip() {
    let (public, private) = crypto::generate_signing_keypair();
    let message = crate::SchedulerGossipMessage::Reconcile(Box::new(query(&public, &private)));
    let decoded = crate::SchedulerGossipMessage::from_bytes(&message.to_bytes().unwrap()).unwrap();
    assert_eq!(decoded, message);
}

#[test]
fn foreign_reply_endpoint_and_expired_query_are_refused() {
    let (public, private) = crypto::generate_signing_keypair();
    let (other_public, other_private) = crypto::generate_signing_keypair();
    let mut foreign = query(&public, &private);
    foreign.reply_endpoint = endpoint(&other_public, &other_private, 9);
    assert!(foreign.sign(&public, &private, NOW).is_err());

    let expired = query(&public, &private);
    assert!(expired.verify(NOW + 30).is_err());
}

#[test]
fn tampered_or_oversized_response_is_refused() {
    let (public, private) = crypto::generate_signing_keypair();
    let mut response = SchedulerReconciliationResponse {
        version: RECONCILIATION_PROTOCOL_VERSION,
        query_id: "reconcile-1".into(),
        responder_endpoint: endpoint(&public, &private, 7),
        event: ReconciliationEvent::AgentAnswer {
            agent_endpoint_id: vec![3; IROH_ENDPOINT_ID_BYTES],
            sealed_response: vec![1; 32],
        },
        responded_at_secs: NOW,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap();
    response.query_id = "substituted".into();
    assert!(response.verify(NOW).is_err());

    let oversized = SchedulerReconciliationResponse {
        version: RECONCILIATION_PROTOCOL_VERSION,
        query_id: "reconcile-1".into(),
        responder_endpoint: endpoint(&public, &private, 7),
        event: ReconciliationEvent::AgentAnswer {
            agent_endpoint_id: vec![3; IROH_ENDPOINT_ID_BYTES],
            sealed_response: vec![0; crate::MAX_AGENT_CONTROL_PAYLOAD_BYTES + 1],
        },
        responded_at_secs: NOW,
        signing_pubkey: String::new(),
        signature: String::new(),
    };
    assert!(oversized.sign(&public, &private, NOW).is_err());
}
