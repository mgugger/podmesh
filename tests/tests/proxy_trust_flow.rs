use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use podmesh_proxy::proxy_grants::ProxyGrantStore;
use podmesh_proxy::restapi::{RestServerOptions, WorkloadRelayBootstrap, spawn_rest_server};
use tokio::sync::watch;

fn signed_record(seed: u8) -> Result<protocol::EndpointRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let (public, private) = crypto::generate_signing_keypair();
    protocol::EndpointRecord {
        version: protocol::ENDPOINT_RECORD_VERSION,
        endpoint_id: vec![seed; protocol::IROH_ENDPOINT_ID_BYTES],
        relay_url: None,
        direct_addresses: vec!["127.0.0.1:4002".to_string()],
        signing_pubkey: String::new(),
        issued_at_secs: now,
        expires_at_secs: now + 600,
        signature: String::new(),
    }
    .sign(&public, &private, now)
}

async fn start_proxy_api(endpoint_record: Arc<RwLock<protocol::EndpointRecord>>) -> Result<String> {
    for _ in 0..16 {
        let port = podmesh_integration_tests::support::allocate_tcp_port();
        let (_peer_tx, peer_rx) = watch::channel(Vec::new());
        spawn_rest_server(RestServerOptions {
            host: "127.0.0.1".to_string(),
            port,
            peer_rx,
            local_peer_id: hex::encode(&endpoint_record.read().unwrap().endpoint_id),
            endpoint_record: endpoint_record.clone(),
            grant_store: ProxyGrantStore::new(),
            relay_bootstrap: None::<WorkloadRelayBootstrap>,
            rate_limit_per_minute: 0,
            metrics: podmesh_metrics::Metrics::noop(),
        })?;
        let url = format!("http://127.0.0.1:{port}");
        for _ in 0..40 {
            if reqwest::get(format!("{url}/healthz"))
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(url);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    anyhow::bail!("proxy REST API did not become ready")
}

#[tokio::test]
async fn explicit_trust_controls_grant_replacement_listing_and_removal() -> Result<()> {
    let key_dir = tempfile::tempdir()?;
    let record = Arc::new(RwLock::new(signed_record(1)?));
    let url = start_proxy_api(record).await?;
    let (owner_public, owner_private) = crypto::generate_signing_keypair();

    let missing =
        podctl::cert::grant_proxy_async_at(&url, &owner_public, &owner_private, 30, key_dir.path())
            .await
            .expect_err("grant must fail before explicit trust");
    ensure!(missing.to_string().contains("is not trusted"));

    let trusted = podctl::cert::trust_proxy_async_at(key_dir.path(), &url, false).await?;
    ensure!(trusted.origin.as_str() == url);
    let grant =
        podctl::cert::grant_proxy_async_at(&url, &owner_public, &owner_private, 30, key_dir.path())
            .await
            .context("trusted proxy grant")?;
    ensure!(grant.owner_pubkey == crypto::b64_encode(&owner_public));

    ensure!(
        podctl::cert::trust_proxy_async_at(key_dir.path(), &url, false)
            .await
            .is_err(),
        "normal trust must not replace an existing binding"
    );
    podctl::cert::trust_proxy_async_at(key_dir.path(), &url, true).await?;
    ensure!(podctl::cert::list_trusted_proxies_at(key_dir.path())?.len() == 1);
    ensure!(podctl::cert::remove_trusted_proxy_at(key_dir.path(), &url)?.is_some());
    ensure!(podctl::cert::remove_trusted_proxy_at(key_dir.path(), &url)?.is_none());
    Ok(())
}

#[tokio::test]
async fn substituted_proxy_identity_is_refused_until_explicit_replacement() -> Result<()> {
    let key_dir = tempfile::tempdir()?;
    let record = Arc::new(RwLock::new(signed_record(2)?));
    let url = start_proxy_api(record.clone()).await?;
    let (owner_public, owner_private) = crypto::generate_signing_keypair();
    podctl::cert::trust_proxy_async_at(key_dir.path(), &url, false).await?;

    *record.write().unwrap() = signed_record(3)?;
    let mismatch =
        podctl::cert::grant_proxy_async_at(&url, &owner_public, &owner_private, 30, key_dir.path())
            .await
            .expect_err("substituted identity must fail");
    ensure!(mismatch.to_string().contains("identity changed"));

    let replaced = podctl::cert::trust_proxy_async_at(key_dir.path(), &url, true).await?;
    ensure!(replaced.endpoint_id_hex == hex::encode([3u8; 32]));
    ensure!(
        podctl::cert::list_trusted_proxies_at(key_dir.path())? == vec![replaced],
        "explicit replacement did not commit the newly observed identity"
    );
    Ok(())
}
