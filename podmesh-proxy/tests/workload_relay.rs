use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, RelayMode, endpoint::presets, tls::CaTlsConfig};
use podmesh_proxy::relay::{DEFAULT_RELAY_KEY_CACHE_CAPACITY, WorkloadRelayConfig};
use rcgen::generate_simple_self_signed;
use rustls_pki_types::CertificateDer;

const TEST_ALPN: &[u8] = b"/podmesh/workload-relay-test/1";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const MESH_SECRET: &str = "test-workload-relay-mesh-secret-0001";

/// A tenant whose relay token this test derives, standing in for a namespace
/// owner key.
fn test_tenant() -> String {
    crypto::b64_encode(&[9u8; 32])
}

#[tokio::test]
async fn relay_only_endpoints_continue_when_direct_transport_is_unavailable() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let certificate = generate_simple_self_signed(vec!["localhost".into()])?;
    let certificate_der = certificate.cert.der().to_vec();
    let certificate_path = temp.path().join("relay-cert.pem");
    let private_key_path = temp.path().join("relay-key.pem");
    fs::write(&certificate_path, certificate.cert.pem())?;
    fs::write(&private_key_path, certificate.signing_key.serialize_pem())?;
    fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600))?;

    let config = WorkloadRelayConfig {
        url: "https://localhost:443".into(),
        mesh_secret: MESH_SECRET.into(),
        relay_tenants: podmesh_proxy::relay::RelayTenantPolicy::All,
        http_listen: "127.0.0.1:0".parse()?,
        https_listen: "127.0.0.1:0".parse()?,
        qad_listen: "127.0.0.1:0".parse()?,
        metrics_listen: "127.0.0.1:0".parse()?,
        tls_certificate: certificate_path,
        tls_private_key: private_key_path,
        key_cache_capacity: DEFAULT_RELAY_KEY_CACHE_CAPACITY,
    };
    let server = podmesh_proxy::relay::start(&config).await?;
    let https_address = server
        .https_addr()
        .context("relay server did not bind HTTPS")?;
    let relay_url = format!("https://localhost:{}", https_address.port()).parse()?;
    let relay_map =
        iroh::RelayMap::from_iter([iroh::RelayConfig::new(relay_url, None).with_auth_token(
            protocol::derive_tenant_relay_token(MESH_SECRET, &test_tenant())?,
        )]);
    let tls = CaTlsConfig::custom_roots([CertificateDer::from(certificate_der)]);

    let server_endpoint = endpoint(relay_map.clone(), tls.clone(), true).await?;
    let client_endpoint = endpoint(relay_map.clone(), tls.clone(), false).await?;
    tokio::time::timeout(TEST_TIMEOUT, server_endpoint.online())
        .await
        .context("server endpoint did not become relay reachable")?;
    tokio::time::timeout(TEST_TIMEOUT, client_endpoint.online())
        .await
        .context("client endpoint did not become relay reachable")?;

    let server_task = tokio::spawn({
        let server_endpoint = server_endpoint.clone();
        async move {
            let incoming = server_endpoint
                .accept()
                .await
                .context("server endpoint closed")?;
            let connection = incoming.await?;
            let (mut send, mut recv) = connection.accept_bi().await?;
            let request = recv.read_to_end(16).await?;
            send.write_all(&request).await?;
            send.finish()?;
            let _ = connection.closed().await;
            Ok::<(), anyhow::Error>(())
        }
    });
    let connection = client_endpoint
        .connect(server_endpoint.addr(), TEST_ALPN)
        .await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(b"relay").await?;
    send.finish()?;
    ensure!(
        recv.read_to_end(16).await? == b"relay",
        "relay echo mismatch"
    );
    connection.close(0u8.into(), b"relay exchange complete");
    server_task.await??;

    // A token derived for another tenant must be refused: scoping is the whole
    // reason the credential is no longer one mesh-wide secret.
    let other_tenant = crypto::b64_encode(&[3u8; 32]);
    let foreign_token = protocol::derive_tenant_relay_token(MESH_SECRET, &other_tenant)?;
    let stolen_token = foreign_token.replace(&other_tenant, &test_tenant());
    let invalid_map = iroh::RelayMap::from_iter([iroh::RelayConfig::new(
        relay_map.urls::<Vec<_>>().into_iter().next().unwrap(),
        None,
    )
    .with_auth_token(stolen_token)]);
    let invalid_endpoint = endpoint(invalid_map, tls, false).await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), invalid_endpoint.online())
            .await
            .is_err()
    );

    invalid_endpoint.close().await;
    client_endpoint.close().await;
    server_endpoint.close().await;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn direct_endpoints_connect_when_relay_transport_is_unavailable() -> Result<()> {
    let server = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![TEST_ALPN.to_vec()])
        .bind()
        .await
        .context("bind direct-only server endpoint")?;
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .context("bind direct-only client endpoint")?;

    let server_task = tokio::spawn({
        let server = server.clone();
        async move {
            let incoming = server.accept().await.context("direct server closed")?;
            let connection = incoming.await?;
            let (mut send, mut recv) = connection.accept_bi().await?;
            let request = recv.read_to_end(16).await?;
            send.write_all(&request).await?;
            send.finish()?;
            let _ = connection.closed().await;
            Ok::<(), anyhow::Error>(())
        }
    });
    let connection = tokio::time::timeout(TEST_TIMEOUT, client.connect(server.addr(), TEST_ALPN))
        .await
        .context("direct-only connection timed out")??;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(b"direct").await?;
    send.finish()?;
    ensure!(
        recv.read_to_end(16).await? == b"direct",
        "direct-only echo mismatch"
    );
    connection.close(0u8.into(), b"direct exchange complete");
    server_task.await??;
    client.close().await;
    server.close().await;
    Ok(())
}

async fn endpoint(relay_map: iroh::RelayMap, tls: CaTlsConfig, accept: bool) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(relay_map))
        .ca_tls_config(tls)
        .clear_ip_transports();
    if accept {
        builder = builder.alpns(vec![TEST_ALPN.to_vec()]);
    }
    builder.bind().await.context("bind relay-only endpoint")
}
