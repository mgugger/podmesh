/// Phase 5 integration test: sealed workload lifecycle without real networking.
///
/// Tests:
///   CustodianRecord store/retrieve
///   WorkloadDispatch serialization roundtrip
///   Coordinator election stability
use podmesh_scheduler::storage::{CustodianRecord, init_custodian_store};
use protocol::machine::{SealedSpec, WorkloadDispatch, SEAL_VERSION_V1};

mod common;

/// Store a custodian record into an ephemeral store and verify retrieval.
#[tokio::test]
async fn test_phase5_custodian_record_store_and_retrieve() {
    init_custodian_store(true).expect("init ephemeral custodian store");

    let record = CustodianRecord::new(
        "deadbeefcafe0011".to_string(),
        "owner-pub".to_string(),
        3,
        2,
        1,
        vec![0xAA, 0xBB, 0xCC],
        vec!["peer-0".to_string(), "peer-1".to_string()],
    );

    let store = podmesh_scheduler::storage::get_custodian_store()
        .expect("custodian store should be initialized");
    store.set_record(&record).expect("set_record");

    let retrieved = store.get_record("deadbeefcafe0011").expect("get_record ok");
    assert!(retrieved.is_some(), "record should be present");
    let r = retrieved.unwrap();
    assert_eq!(r.manifest_id, "deadbeefcafe0011");
    assert_eq!(r.share_index, 1);
    assert_eq!(r.wrapped_share, vec![0xAA, 0xBB, 0xCC]);
}

/// WorkloadDispatch roundtrip serialization.
#[test]
fn test_phase5_workload_dispatch_serde_roundtrip() {
    let spec = SealedSpec {
        manifest_id: "testid00".to_string(),
        owner_pubkey: "pub".to_string(),
        ciphertext: vec![1, 2, 3],
        nonce: vec![0u8; 24],
        kfrag_count: 3,
        kfrag_threshold: 2,
        sealed_at_secs: 1_700_000_000,
        submission_version: SEAL_VERSION_V1,
        replica_count: 1,
    };
    let dispatch = WorkloadDispatch {
        sealed_spec: spec,
        custodian_peers: vec!["p1".to_string(), "p2".to_string(), "p3".to_string()],
        coordinator_sig: "coord-sig".to_string(),
        worker_wrapped_shares: vec![],
        coordinator_peer_id: String::new(),
        assignment_token: String::new(),
        assigned_at_secs: 0,
    };

    let bytes = dispatch.to_bytes();
    let decoded = WorkloadDispatch::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.custodian_peers.len(), 3);
    assert_eq!(decoded.coordinator_sig, "coord-sig");
    assert_eq!(decoded.sealed_spec.manifest_id, "testid00");
}

/// Verify coordinator election is stable — same inputs always elect same coordinator.
#[test]
fn test_phase5_coordinator_election_stability() {
    use podmesh_scheduler::custodian::coordinator::{elect_coordinator, is_coordinator};

    let peers = vec![
        "peer-custodian-A".to_string(),
        "peer-custodian-B".to_string(),
        "peer-custodian-C".to_string(),
    ];
    let manifest_id = "cafebabe00112233";

    let elected = elect_coordinator(manifest_id, &peers).unwrap();
    assert_eq!(elect_coordinator(manifest_id, &peers), Some(elected));
    assert!(is_coordinator(manifest_id, elected, &peers));
    for p in &peers {
        if p != elected {
            assert!(!is_coordinator(manifest_id, p, &peers));
        }
    }
}
