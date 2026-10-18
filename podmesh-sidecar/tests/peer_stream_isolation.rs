//! A failed anti-entropy operation must not reset independent service streams.
mod support;
use anyhow::{Context, Result, ensure};
use iroh::{Endpoint, endpoint::presets};
use iroh_support::NodeIdentity;
use podmesh_sidecar::{
    peer_connector::{PeerConnector, system_clock},
    service_cache::{ServiceCache, ServiceKey},
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_sync_does_not_reset_an_established_service_tunnel() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let authority = support::Authority::new();
        let now = support::now();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let catalog = authority.catalog(listener.local_addr()?.port());
        let application = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut bytes = [0; 5];
            loop {
                if socket.read_exact(&mut bytes).await.is_err() {
                    break;
                }
                socket.write_all(&bytes).await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let mut peers = Vec::new();
        let mut caches = Vec::new();
        for index in 0..2 {
            let node = Arc::new(NodeIdentity::ephemeral());
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(node.transport_secret().clone())
                .bind_addr("127.0.0.1:0")?
                .clear_relay_transports()
                .bind()
                .await?;
            let record = authority.record(
                &catalog,
                &node,
                endpoint
                    .addr()
                    .ip_addrs()
                    .map(ToString::to_string)
                    .collect(),
                index,
                now,
                120,
            );
            let cache = Arc::new(Mutex::new(ServiceCache::new(authority.owner.clone())?));
            peers.push(PeerConnector::start(
                endpoint,
                node,
                record,
                cache.clone(),
                system_clock(),
                "127.0.0.1".parse()?,
            )?);
            caches.push(cache);
        }
        let result: Result<()> = async {
            let remote = peers[1].record()?;
            peers[0].sync_with(&remote).await?;
            let key = ServiceKey {
                manifest_id: catalog.manifest_id.clone(),
                name: "api".into(),
            };
            let mut tunnel = peers[0].open_service(&key, 80, true).await?;
            tunnel.send.write_all(b"first").await?;
            let mut reply = [0; 5];
            tunnel.recv.read_exact(&mut reply).await?;
            ensure!(&reply == b"first");

            // Owner control selects a conflicting catalog while the original
            // peer remains connected. Its valid sync response is then refused.
            // This is a per-operation failure, not revocation of the existing
            // tunnel's still-valid endpoint-bound credentials.
            let mut replacement = catalog.clone();
            replacement.revision_id = protocol::revision_id(b"other revision");
            caches[0]
                .lock()
                .unwrap()
                .replace_catalog(authority.sign_catalog(replacement))?;
            let error = peers[0].sync_with(&remote).await.unwrap_err();
            ensure!(
                error.to_string().contains("catalog revision/conflict"),
                "unexpected failure: {error:#}"
            );
            caches[0].lock().unwrap().replace_catalog(catalog.clone())?;
            peers[0]
                .sync_with(&remote)
                .await
                .context("peer transport unusable after failed sync")?;

            tunnel
                .send
                .write_all(b"after")
                .await
                .context("active tunnel reset by failed sync")?;
            tunnel
                .recv
                .read_exact(&mut reply)
                .await
                .context("active tunnel response reset by failed sync")?;
            ensure!(&reply == b"after");
            Ok(())
        }
        .await;
        for peer in peers {
            peer.shutdown().await?;
        }
        application.abort();
        let _ = application.await;
        result
    })
    .await?
}
