use protocol::machine::{
    AppliedManifest, KeyValue, OperationType, SignatureScheme, build_applied_manifest,
    root_as_applied_manifest,
};

#[test]
fn test_applied_manifest_owner_fields_roundtrip() {
    let id = "id-123";
    let operation_id = "op-1";
    let origin_peer = "12D3KooW...";
    let owner_pub = vec![1u8, 2u8, 3u8];
    let signature = vec![9u8, 8u8, 7u8];
    let manifest_json = "{\"k\":\"v\"}";
    let manifest_kind = "Test";
    let timestamp = 123456789u64;

    let manifest = AppliedManifest {
        id: id.into(),
        operation_id: operation_id.into(),
        origin_peer: origin_peer.into(),
        owner_pubkey: owner_pub.clone(),
        signature_scheme: SignatureScheme::Ed25519,
        signature: signature.clone(),
        manifest_json: manifest_json.into(),
        manifest_kind: manifest_kind.into(),
        labels: vec![KeyValue {
            key: "env".into(),
            value: "test".into(),
        }],
        timestamp,
        operation: OperationType::Apply,
        ttl_secs: 3600,
        content_hash: "chash".into(),
    };

    let buf = build_applied_manifest(manifest);

    let parsed = root_as_applied_manifest(&buf).expect("parse");
    assert_eq!(parsed.id().unwrap(), id);
    assert_eq!(parsed.owner_pubkey().unwrap().len(), 3);
    assert_eq!(parsed.signature().unwrap().len(), 3);
}
