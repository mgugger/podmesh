use std::{
    fs,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    os::unix::fs::PermissionsExt,
    time::Duration,
};

use podmesh_scheduler::relay::{CertificateMode, DenyAll, MachineRelayConfig};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn integrated_relay_listeners_health_metrics_and_shutdown_work() {
    let temp = tempfile::tempdir().unwrap();
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let certificate_pem = certified.cert.pem();
    let certificate_path = temp.path().join("relay.crt");
    let private_key_path = temp.path().join("relay.key");
    fs::write(&certificate_path, &certificate_pem).unwrap();
    fs::write(&private_key_path, certified.signing_key.serialize_pem()).unwrap();
    fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600)).unwrap();

    let metrics_listen = available_tcp_address();
    let config = MachineRelayConfig {
        audience: "https://relay.example.test".into(),
        trusted_issuer_keys: vec![crypto::b64_encode(&[7; 32])],
        http_listen: (Ipv4Addr::LOCALHOST, 0).into(),
        https_listen: (Ipv4Addr::LOCALHOST, 0).into(),
        qad_listen: (Ipv4Addr::LOCALHOST, 0).into(),
        metrics_listen,
        certificate_mode: CertificateMode::Manual,
        tls_certificate: Some(certificate_path),
        tls_private_key: Some(private_key_path),
        acme_domains: Vec::new(),
        acme_contact: None,
        acme_cache_dir: temp.path().join("acme"),
        acme_staging: true,
        key_cache_capacity: 128,
    };

    let server = podmesh_scheduler::relay::start(config, DenyAll)
        .await
        .unwrap();
    let http_address = server.http_addr().unwrap();
    let https_address = server.https_addr().unwrap();
    assert_ne!(http_address, https_address);
    assert!(server.quic_addr().is_some());

    let root = reqwest::Certificate::from_pem(certificate_pem.as_bytes()).unwrap();
    let client = reqwest::Client::builder()
        .add_root_certificate(root)
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap();
    let health = client
        .get(format!("https://{https_address}/healthz"))
        .send()
        .await
        .unwrap();
    assert!(health.status().is_success());

    let probe = client
        .get(format!("http://{http_address}/generate_204"))
        .send()
        .await
        .unwrap();
    assert_eq!(probe.status(), reqwest::StatusCode::NO_CONTENT);

    let metrics = client
        .get(format!("http://{metrics_listen}/metrics"))
        .send()
        .await
        .unwrap();
    assert!(metrics.status().is_success());

    tokio::time::timeout(SHUTDOWN_TIMEOUT, server.shutdown())
        .await
        .unwrap()
        .unwrap();
}

fn available_tcp_address() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap()
}
