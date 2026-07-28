use protocol::ENDPOINT_RECORD_VERSION;

use super::*;

const NOW: u64 = 1_000;

fn endpoint_record(endpoint_id: EndpointId) -> String {
    let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
    let record = EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: endpoint_id.as_bytes().to_vec(),
        relay_url: Some("https://relay.example.test".into()),
        direct_addresses: vec!["127.0.0.1:4000".into()],
        signing_pubkey: String::new(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 300,
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap();
    crypto::b64_encode(&record.to_bytes(NOW).unwrap())
}

fn config() -> MachineConfig {
    MachineConfig {
        scheduler_endpoints: vec![endpoint_record(iroh::SecretKey::generate().public())],
        relay_urls: vec!["https://relay.example.test".into()],
        ..MachineConfig::default()
    }
}

#[test]
fn valid_scheduler_and_relay_configuration_passes() {
    let validated = config().validate(NOW).unwrap();
    assert_eq!(validated.scheduler_endpoints.len(), 1);
    assert_eq!(validated.scheduler_ids.len(), 1);
    assert!(validated.relay_urls.contains("https://relay.example.test/"));
}

#[test]
fn missing_duplicate_and_unreachable_schedulers_fail_closed() {
    let mut missing = config();
    missing.scheduler_endpoints.clear();
    assert!(missing.validate(NOW).is_err());

    let record = config().scheduler_endpoints.remove(0);
    let mut duplicate = config();
    duplicate.scheduler_endpoints = vec![record.clone(), record];
    duplicate.max_scheduler_attachments = 2;
    assert!(duplicate.validate(NOW).is_err());

    let bytes = crypto::b64_decode(&endpoint_record(iroh::SecretKey::generate().public())).unwrap();
    let mut record = EndpointRecord::from_bytes(&bytes, NOW).unwrap();
    record.direct_addresses.clear();
    let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
    record = record.sign(&public, &private, NOW).unwrap();
    let mut unreachable = config();
    unreachable.scheduler_endpoints = vec![crypto::b64_encode(&record.to_bytes(NOW).unwrap())];
    assert!(unreachable.validate(NOW).is_err());
}

#[test]
fn relay_reconnect_and_endpoint_bounds_fail_closed() {
    let mut missing_relay = config();
    missing_relay.relay_urls.clear();
    assert!(missing_relay.validate(NOW).is_err());

    let mut invalid_backoff = config();
    invalid_backoff.reconnect_initial_ms = 2_000;
    invalid_backoff.reconnect_max_ms = 1_000;
    assert!(invalid_backoff.validate(NOW).is_err());

    let mut zero_queries = config();
    zero_queries.max_seen_queries = 0;
    assert!(zero_queries.validate(NOW).is_err());

    let mut invalid_streams = config();
    invalid_streams.max_concurrent_uni_streams = 0;
    assert!(invalid_streams.validate(NOW).is_err());

    let mut invalid_windows = config();
    invalid_windows.connection_receive_window_bytes =
        invalid_windows.stream_receive_window_bytes - 1;
    assert!(invalid_windows.validate(NOW).is_err());
}
