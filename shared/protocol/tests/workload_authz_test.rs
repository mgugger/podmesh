use biscuit_auth::KeyPair;
use protocol::{
    BiscuitWorkloadOperation as Operation, WorkloadAuthorizationContext, WorkloadCapabilityClaims,
    authorize_workload_biscuit, mint_workload_biscuit,
};

const NOW: u64 = 30_000;

fn claims(operation: Operation) -> WorkloadCapabilityClaims {
    WorkloadCapabilityClaims {
        tenant_owner: "tenant-a".into(),
        workload_id: "workload-a".into(),
        subject_endpoint: "sidecar-a".into(),
        audience_endpoint: "proxy-a".into(),
        role: "sidecar".into(),
        operation,
        issued_at_secs: NOW,
        expires_at_secs: NOW + 60,
        token_id: "token-a".into(),
        ingress_path_prefixes: if operation == Operation::Ingress {
            vec!["/api/".into()]
        } else {
            Vec::new()
        },
        egress_host: (operation == Operation::Egress).then(|| "example.test".into()),
        egress_port: (operation == Operation::Egress).then_some(443),
    }
}

fn context(operation: Operation) -> WorkloadAuthorizationContext {
    WorkloadAuthorizationContext {
        tenant_owner: "tenant-a".into(),
        workload_id: "workload-a".into(),
        subject_endpoint: "sidecar-a".into(),
        audience_endpoint: "proxy-a".into(),
        role: "sidecar".into(),
        operation,
        now_secs: NOW + 1,
        ingress_path: (operation == Operation::Ingress).then(|| "/api/items".into()),
        egress_host: (operation == Operation::Egress).then(|| "example.test".into()),
        egress_port: (operation == Operation::Egress).then_some(443),
    }
}

fn mint(operation: Operation) -> (Vec<u8>, biscuit_auth::PublicKey) {
    let root = KeyPair::new();
    let token = mint_workload_biscuit(&root, &claims(operation)).unwrap();
    (token, root.public())
}

#[test]
fn mesh_join_binds_tenant_endpoint_audience_role_and_workload() {
    let (token, root) = mint(Operation::MeshJoin);
    authorize_workload_biscuit(&token, root, &context(Operation::MeshJoin)).unwrap();

    let mut wrong = context(Operation::MeshJoin);
    wrong.tenant_owner = "tenant-b".into();
    assert!(authorize_workload_biscuit(&token, root, &wrong).is_err());

    let mut wrong = context(Operation::MeshJoin);
    wrong.subject_endpoint = "sidecar-b".into();
    assert!(authorize_workload_biscuit(&token, root, &wrong).is_err());
}

#[test]
fn operation_cannot_be_substituted() {
    let (token, root) = mint(Operation::Registration);
    assert!(authorize_workload_biscuit(&token, root, &context(Operation::ProxyDiscovery)).is_err());
}

#[test]
fn ingress_requires_allowed_path_prefix() {
    let (token, root) = mint(Operation::Ingress);
    authorize_workload_biscuit(&token, root, &context(Operation::Ingress)).unwrap();

    let mut denied = context(Operation::Ingress);
    denied.ingress_path = Some("/admin".into());
    assert!(authorize_workload_biscuit(&token, root, &denied).is_err());
}

#[test]
fn egress_requires_exact_host_and_port() {
    let (token, root) = mint(Operation::Egress);
    authorize_workload_biscuit(&token, root, &context(Operation::Egress)).unwrap();

    let mut denied = context(Operation::Egress);
    denied.egress_port = Some(80);
    assert!(authorize_workload_biscuit(&token, root, &denied).is_err());

    let mut denied = context(Operation::Egress);
    denied.egress_host = Some("other.test".into());
    assert!(authorize_workload_biscuit(&token, root, &denied).is_err());
}

#[test]
fn expired_and_wrong_root_tokens_fail_closed() {
    let (token, root) = mint(Operation::MeshJoin);
    let mut expired = context(Operation::MeshJoin);
    expired.now_secs = NOW + 61;
    assert!(authorize_workload_biscuit(&token, root, &expired).is_err());

    let other_root = KeyPair::new();
    assert!(
        authorize_workload_biscuit(&token, other_root.public(), &context(Operation::MeshJoin))
            .is_err()
    );
}

#[test]
fn token_size_is_bounded_before_parsing() {
    let root = KeyPair::new();
    let oversized = vec![0; protocol::MAX_BISCUIT_TOKEN_BYTES + 1];
    assert!(
        authorize_workload_biscuit(&oversized, root.public(), &context(Operation::MeshJoin),)
            .is_err()
    );
}
