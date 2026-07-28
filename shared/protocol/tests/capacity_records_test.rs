use protocol::{
    CAPACITY_PROTOCOL_VERSION, CapacityOffer, CapacityQuery, ENDPOINT_RECORD_VERSION,
    EndpointRecord, IROH_ENDPOINT_ID_BYTES,
};

const NOW: u64 = 10_000;

fn signed_endpoint(public: &[u8], private: &[u8], endpoint_byte: u8) -> EndpointRecord {
    EndpointRecord {
        version: ENDPOINT_RECORD_VERSION,
        endpoint_id: vec![endpoint_byte; IROH_ENDPOINT_ID_BYTES],
        relay_url: Some("https://relay.example.test".into()),
        direct_addresses: vec!["192.0.2.10:4000".into()],
        signing_pubkey: String::new(),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 300,
        signature: String::new(),
    }
    .sign(public, private, NOW)
    .unwrap()
}

fn signed_query() -> CapacityQuery {
    let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
    CapacityQuery {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: "query-1".into(),
        nonce: "nonce-1".into(),
        cpu_milli: 500,
        memory_bytes: 512 * 1024 * 1024,
        storage_bytes: 1024 * 1024 * 1024,
        required_capabilities: vec!["region:eu".into()],
        excluded_endpoint_ids: vec![vec![9; IROH_ENDPOINT_ID_BYTES]],
        reply_endpoint: signed_endpoint(&public, &private, 1),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap()
}

fn signed_offer() -> CapacityOffer {
    let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
    CapacityOffer {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: "query-1".into(),
        agent_endpoint: signed_endpoint(&public, &private, 2),
        kem_pubkey: crypto::b64_encode(&[7; 32]),
        available_cpu_milli: 2_000,
        available_memory_bytes: 2 * 1024 * 1024 * 1024,
        available_storage_bytes: 10 * 1024 * 1024 * 1024,
        capabilities: vec!["region:eu".into(), "multi-workload".into()],
        issued_at_secs: NOW,
        expires_at_secs: NOW + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    }
    .sign(&public, &private, NOW)
    .unwrap()
}

#[test]
fn signed_capacity_records_roundtrip() {
    let query = signed_query();
    let offer = signed_offer();
    assert_eq!(
        CapacityQuery::from_bytes(&query.to_bytes(NOW).unwrap(), NOW).unwrap(),
        query
    );
    assert_eq!(
        CapacityOffer::from_bytes(&offer.to_bytes(NOW).unwrap(), NOW).unwrap(),
        offer
    );
}

#[test]
fn signatures_cover_resource_and_transport_identity() {
    let mut query = signed_query();
    query.cpu_milli += 1;
    assert!(query.verify(NOW).is_err());

    let mut offer = signed_offer();
    offer.agent_endpoint.endpoint_id[0] ^= 1;
    assert!(offer.verify(NOW).is_err());
}

#[test]
fn signer_must_match_nested_endpoint_record() {
    let (endpoint_public, endpoint_private) = crypto::ensure_keypair_ephemeral().unwrap();
    let (other_public, other_private) = crypto::ensure_keypair_ephemeral().unwrap();
    let query = CapacityQuery {
        version: CAPACITY_PROTOCOL_VERSION,
        query_id: "query-2".into(),
        nonce: "nonce-2".into(),
        cpu_milli: 1,
        memory_bytes: 1,
        storage_bytes: 1,
        required_capabilities: Vec::new(),
        excluded_endpoint_ids: Vec::new(),
        reply_endpoint: signed_endpoint(&endpoint_public, &endpoint_private, 3),
        issued_at_secs: NOW,
        expires_at_secs: NOW + 10,
        signing_pubkey: String::new(),
        signature: String::new(),
    };
    assert!(query.sign(&other_public, &other_private, NOW).is_err());
}

#[test]
fn rejects_duplicate_or_unbounded_query_fields() {
    let mut query = signed_query();
    query.required_capabilities.push("region:eu".into());
    assert!(query.verify(NOW).is_err());

    let mut query = signed_query();
    query
        .excluded_endpoint_ids
        .push(vec![9; IROH_ENDPOINT_ID_BYTES]);
    assert!(query.verify(NOW).is_err());

    let mut query = signed_query();
    query.excluded_endpoint_ids = vec![vec![1; IROH_ENDPOINT_ID_BYTES - 1]];
    assert!(query.verify(NOW).is_err());
}

#[test]
fn rejects_expired_records_and_invalid_kem_key() {
    let query = signed_query();
    assert!(query.verify(NOW + 11).is_err());

    let mut offer = signed_offer();
    offer.kem_pubkey = crypto::b64_encode(&[1; 31]);
    assert!(offer.verify(NOW).is_err());
}

#[test]
fn rejects_oversized_encoded_input_before_decoding() {
    let bytes = vec![0; protocol::MAX_CAPACITY_MESSAGE_BYTES + 1];
    assert!(CapacityQuery::from_bytes(&bytes, NOW).is_err());
    assert!(CapacityOffer::from_bytes(&bytes, NOW).is_err());
}
