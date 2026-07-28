use std::net::{Ipv4Addr, SocketAddr};

use iroh::{EndpointAddr, SecretKey};
use protocol::{CAPACITY_PROTOCOL_VERSION, ENDPOINT_RECORD_VERSION, EndpointRecord};

use super::*;

const NOW: u64 = 10_000;

fn criteria() -> CapacityCriteria {
    CapacityCriteria {
        cpu_milli: 500,
        memory_bytes: 512 * 1024 * 1024,
        storage_bytes: 1024 * 1024 * 1024,
        required_capabilities: vec!["linux".into()],
        excluded_endpoint_ids: Vec::new(),
    }
}

fn reply_address(identity: &SchedulerIdentity) -> EndpointAddr {
    EndpointAddr::new(identity.endpoint_id())
        .with_ip_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 4000)))
}

fn signed_offer(query_id: &str, transport: &SecretKey, available_cpu_milli: u32) -> CapacityOffer {
    let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
    let endpoint = EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: transport.public().as_bytes().to_vec(),
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4100".into()],
        signing_pubkey: String::new(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 10,
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap();
    CapacityOffer {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: query_id.into(),
        agent_endpoint: endpoint,
        kem_pubkey: crypto::b64_encode(&[5; 32]),
        available_cpu_milli,
        available_memory_bytes: 1024 * 1024 * 1024,
        available_storage_bytes: 2 * 1024 * 1024 * 1024,
        capabilities: vec!["linux".into()],
        issued_at_secs: NOW,
        expires_at_secs: NOW + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap()
}

#[tokio::test]
async fn equivalent_queries_coalesce_and_complete_idempotently() {
    let identity = SchedulerIdentity::ephemeral().unwrap();
    let manager = QueryManager::new(2, 4, Duration::from_secs(5));
    let address = reply_address(&identity);
    let first = manager
        .begin(criteria(), &identity, &address, NOW)
        .await
        .unwrap();
    let second = manager
        .begin(criteria(), &identity, &address, NOW)
        .await
        .unwrap();
    assert!(first.newly_created);
    assert!(!second.newly_created);
    assert_eq!(first.query.query_id, second.query.query_id);

    let transport = SecretKey::generate();
    manager
        .submit_offer(
            signed_offer(&first.query.query_id, &transport, 700),
            transport.public(),
            NOW,
        )
        .await
        .unwrap();
    let selected = manager.finish(&first.query.query_id, NOW).await.unwrap();
    assert_eq!(
        manager.finish(&second.query.query_id, NOW).await,
        Some(selected)
    );
}

#[tokio::test]
async fn offers_are_transport_bound_deduplicated_and_selected_deterministically() {
    let identity = SchedulerIdentity::ephemeral().unwrap();
    let manager = QueryManager::new(2, 4, Duration::from_secs(5));
    let begun = manager
        .begin(criteria(), &identity, &reply_address(&identity), NOW)
        .await
        .unwrap();
    let larger = SecretKey::generate();
    let best_fit = SecretKey::generate();
    let larger_offer = signed_offer(&begun.query.query_id, &larger, 900);
    let best_offer = signed_offer(&begun.query.query_id, &best_fit, 600);
    assert!(
        manager
            .submit_offer(larger_offer.clone(), larger.public(), NOW)
            .await
            .unwrap()
    );
    assert!(
        !manager
            .submit_offer(larger_offer, larger.public(), NOW)
            .await
            .unwrap()
    );
    assert!(
        manager
            .submit_offer(best_offer.clone(), SecretKey::generate().public(), NOW)
            .await
            .is_err()
    );
    manager
        .submit_offer(best_offer.clone(), best_fit.public(), NOW)
        .await
        .unwrap();
    assert_eq!(
        manager.finish(&begun.query.query_id, NOW).await,
        Some(best_offer)
    );
}

#[tokio::test]
async fn pending_bounds_exclusions_and_deadlines_fail_closed() {
    let identity = SchedulerIdentity::ephemeral().unwrap();
    let manager = QueryManager::new(1, 1, Duration::from_secs(5));
    let address = reply_address(&identity);
    let excluded = SecretKey::generate();
    let mut requested = criteria();
    requested.excluded_endpoint_ids = vec![excluded.public().as_bytes().to_vec()];
    let begun = manager
        .begin(requested, &identity, &address, NOW)
        .await
        .unwrap();
    assert!(
        manager
            .submit_offer(
                signed_offer(&begun.query.query_id, &excluded, 700),
                excluded.public(),
                NOW,
            )
            .await
            .is_err()
    );
    let mut different = criteria();
    different.cpu_milli = 750;
    assert!(
        manager
            .begin(different, &identity, &address, NOW)
            .await
            .is_err()
    );

    let eligible = SecretKey::generate();
    assert!(
        manager
            .submit_offer(
                signed_offer(&begun.query.query_id, &eligible, 700),
                eligible.public(),
                NOW + 6,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn empty_query_returns_no_offer_and_restart_has_no_pending_state() {
    let identity = SchedulerIdentity::ephemeral().unwrap();
    let manager = QueryManager::new(2, 2, Duration::from_secs(5));
    let begun = manager
        .begin(criteria(), &identity, &reply_address(&identity), NOW)
        .await
        .unwrap();
    assert_eq!(manager.finish(&begun.query.query_id, NOW).await, None);
    assert_eq!(manager.pending_len().await, 1);

    let restarted = QueryManager::new(2, 2, Duration::from_secs(5));
    assert_eq!(restarted.pending_len().await, 0);
    let retried = restarted
        .begin(criteria(), &identity, &reply_address(&identity), NOW + 1)
        .await
        .unwrap();
    assert_ne!(retried.query.query_id, begun.query.query_id);
}
