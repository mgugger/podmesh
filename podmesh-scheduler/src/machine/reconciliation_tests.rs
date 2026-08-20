use std::{collections::HashSet, time::Duration};

use iroh::EndpointId;

use super::ReconciliationRegistry;

const NOW: u64 = 1_700_000_000;

fn response(
    scheduler: EndpointId,
    event: protocol::ReconciliationEvent,
) -> protocol::SchedulerReconciliationResponse {
    let (public, private) = crypto::generate_signing_keypair();
    let endpoint = protocol::EndpointRecord {
        version: protocol::ENDPOINT_RECORD_VERSION,
        endpoint_id: scheduler.as_bytes().to_vec(),
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4000".into()],
        signing_pubkey: String::new(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 60,
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap();
    protocol::SchedulerReconciliationResponse {
        version: protocol::RECONCILIATION_PROTOCOL_VERSION,
        query_id: "query".into(),
        responder_endpoint: endpoint,
        event,
        responded_at_secs: NOW,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap()
}

#[tokio::test]
async fn answers_are_deduplicated_and_pending_state_is_removed() {
    let scheduler = iroh::SecretKey::generate().public();
    let agent = iroh::SecretKey::generate().public();
    let registry = ReconciliationRegistry::new(2);
    let notify = registry
        .begin("query".into(), HashSet::from([scheduler]))
        .await
        .unwrap();
    let answer = protocol::ReconciliationEvent::AgentAnswer {
        agent_endpoint_id: agent.as_bytes().to_vec(),
        sealed_response: vec![1, 2, 3],
    };
    registry
        .record(scheduler, response(scheduler, answer.clone()))
        .await
        .unwrap();
    registry
        .record(scheduler, response(scheduler, answer))
        .await
        .unwrap();
    registry
        .record(
            scheduler,
            response(scheduler, protocol::ReconciliationEvent::Complete),
        )
        .await
        .unwrap();
    let outcome = registry
        .finish("query", notify, Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(outcome.answers.len(), 1);
    assert!(outcome.unreachable_schedulers.is_empty());
    assert!(registry.is_empty().await);
}

#[tokio::test]
async fn an_unexpected_scheduler_cannot_complete_a_query() {
    let expected = iroh::SecretKey::generate().public();
    let unexpected = iroh::SecretKey::generate().public();
    let registry = ReconciliationRegistry::new(1);
    registry
        .begin("query".into(), HashSet::from([expected]))
        .await
        .unwrap();
    let result = registry
        .record(
            unexpected,
            response(unexpected, protocol::ReconciliationEvent::Complete),
        )
        .await;
    assert!(result.is_err());
}
