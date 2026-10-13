//! Three real Iroh endpoints and TCP apps on loopback. No containers, relays,
//! nftables, host DNS mutation, schedulers, agents or proxies are started.
mod support;
use anyhow::{Context, Result, ensure};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, RecordType},
};
use iroh::{Endpoint, endpoint::presets};
use iroh_support::NodeIdentity;
use podmesh_sidecar::{
    dns::{DnsForwarder, DnsLimits, LocalDns},
    peer_connector::{Clock, PeerConnector},
    service_cache::{ServiceCache, ServiceKey},
};
use protocol::{
    WorkloadPayload,
    peer_control::{PEER_ALPN, PeerOperation, PeerRequest},
};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};

async fn application(listener: TcpListener, index: usize) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut handlers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(_) = handlers.join_next(), if !handlers.is_empty() => {},
                incoming = listener.accept() => {
                    let Ok((mut stream, _)) = incoming else { break; };
                    handlers.spawn(async move {
                        let mut request = Vec::new();
                        stream.read_to_end(&mut request).await.unwrap();
                        stream.write_all(format!("replica-{index}:").as_bytes()).await.unwrap();
                        stream.write_all(&request).await.unwrap();
                        stream.shutdown().await.unwrap();
                    });
                }
            }
        }
    })
}

async fn query_address(server: SocketAddr, name: &str) -> Result<Ipv4Addr> {
    let mut query = Message::new(17, MessageType::Query, OpCode::Query);
    query.metadata.recursion_desired = true;
    query
        .queries
        .push(Query::query(Name::from_ascii(name)?, RecordType::A));
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket.connect(server).await?;
    socket.send(&query.to_vec()?).await?;
    let mut bytes = vec![0; 4096];
    let size = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut bytes)).await??;
    let message = Message::from_vec(&bytes[..size])?;
    ensure!(
        message.metadata.response_code == ResponseCode::NoError,
        "internal DNS did not resolve"
    );
    match message.answers.first().context("DNS answer missing")?.data {
        RData::A(value) => Ok(value.0),
        _ => anyhow::bail!("DNS did not return IPv4"),
    }
}

async fn call(peer: &PeerConnector, address: Ipv4Addr) -> Result<(String, String)> {
    let mut tunnel = peer.open_address(address, 80, true).await?;
    let remote = tunnel.remote_workload_id.clone();
    tunnel.send.write_all(b"hello").await?;
    tunnel.send.finish()?;
    let data =
        tokio::time::timeout(Duration::from_secs(5), tunnel.recv.read_to_end(1024)).await??;
    Ok((remote, String::from_utf8(data)?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_peers_discover_resolve_route_retry_and_expire_without_infrastructure() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(30), async {
        let authority = support::Authority::new();
        let now = support::now();
        let time = Arc::new(AtomicU64::new(now));
        let clock: Clock = {
            let time = time.clone();
            Arc::new(move || time.load(Ordering::SeqCst))
        };
        // Loopback aliases model each workload's private application interface.
        // The owner-authorized Service targetPort is identical across replicas.
        let first = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 2), 0)).await?;
        let port = first.local_addr()?.port();
        let listeners = vec![
            first,
            TcpListener::bind((Ipv4Addr::new(127, 0, 0, 3), port)).await?,
            TcpListener::bind((Ipv4Addr::new(127, 0, 0, 4), port)).await?,
        ];
        let catalog = authority.catalog(port);
        let key = ServiceKey {
            manifest_id: catalog.manifest_id.clone(),
            name: "api".into(),
        };
        let mut peers = Vec::new();
        let mut caches = Vec::new();
        let mut apps = Vec::new();
        for (index, listener) in listeners.into_iter().enumerate() {
            let host = listener.local_addr()?.ip();
            apps.push(application(listener, index).await);
            let node = Arc::new(NodeIdentity::ephemeral());
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(node.transport_secret().clone())
                .bind_addr("127.0.0.1:0")?
                .clear_relay_transports()
                .bind()
                .await?;
            let addresses = endpoint
                .addr()
                .ip_addrs()
                .map(ToString::to_string)
                .collect();
            let lifetime = if index == 1 { 2 } else { 120 };
            let record = authority.record(&catalog, &node, addresses, index as u32, now, lifetime);
            let cache = Arc::new(Mutex::new(ServiceCache::new(authority.owner.clone())?));
            let std::net::IpAddr::V4(host) = host else {
                unreachable!()
            };
            peers.push(Some(PeerConnector::start(
                endpoint,
                node,
                record,
                cache.clone(),
                clock.clone(),
                host,
            )?));
            caches.push(cache);
        }
        let b = peers[1].as_ref().unwrap().record()?;
        let c = peers[2].as_ref().unwrap().record()?;
        // Sparse introduction: B knows C; A knows only B. Signed records propagate.
        peers[1].as_ref().unwrap().sync_with(&c).await?;
        peers[0].as_ref().unwrap().sync_with(&b).await?;
        assert_eq!(caches[0].lock().unwrap().len(), 3);
        let no_upstream = UdpSocket::bind("127.0.0.1:0").await?;
        let upstream_address = no_upstream.local_addr()?;
        let dns = LocalDns::bind(
            "127.0.0.1:0".parse()?,
            caches[0].clone(),
            Arc::new(DnsForwarder::new(vec![upstream_address])?),
            clock.clone(),
            DnsLimits::default(),
        )
        .await?;
        let address = query_address(dns.address(), &key.fqdn()?).await?;
        let (remote_b, response_b) = call(peers[0].as_ref().unwrap(), address).await?;
        let (remote_c, response_c) = call(peers[0].as_ref().unwrap(), address).await?;
        assert_eq!(remote_b, b.credential.delegation.scope.workload_id);
        assert_eq!(remote_c, c.credential.delegation.scope.workload_id);
        assert_eq!(response_b, "replica-1:hello");
        assert_eq!(response_c, "replica-2:hello");
        assert!(
            peers[0]
                .as_ref()
                .unwrap()
                .open_address(address, 22, true)
                .await
                .is_err()
        );
        assert!(
            peers[0]
                .as_ref()
                .unwrap()
                .open_address(Ipv4Addr::new(198, 18, 0, 250), 80, true)
                .await
                .is_err()
        );
        // Stop B but retain its not-yet-expired advertisement. Setup retry uses C.
        peers[1].take().unwrap().shutdown().await?;
        let (remote, reply) = call(peers[0].as_ref().unwrap(), address).await?;
        assert_eq!(remote, c.credential.delegation.scope.workload_id);
        assert_eq!(reply, "replica-2:hello");
        time.store(now + 3, Ordering::SeqCst);
        assert_eq!(
            query_address(dns.address(), &key.fqdn()?).await?,
            address,
            "replica churn changed the VIP"
        );
        let candidates = caches[0].lock().unwrap().candidates(&key, now + 3, None);
        assert!(candidates.iter().all(|record| record.replica_index != 1));
        // A foreign tenant is refused on the wire, not just by caller-side cache validation.
        let foreign = support::Authority::new();
        let foreign_catalog = foreign.catalog(port);
        let foreign_node = NodeIdentity::ephemeral();
        let foreign_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(foreign_node.transport_secret().clone())
            .bind_addr("127.0.0.1:0")?
            .clear_relay_transports()
            .bind()
            .await?;
        let foreign_record = foreign.record(
            &foreign_catalog,
            &foreign_node,
            foreign_endpoint
                .addr()
                .ip_addrs()
                .map(ToString::to_string)
                .collect(),
            0,
            now + 3,
            120,
        );
        let target = peers[0].as_ref().unwrap().record()?;
        let connection = foreign_endpoint
            .connect(
                iroh_support::endpoint_addr(&target.endpoint, now + 3)?,
                PEER_ALPN,
            )
            .await?;
        let request = PeerRequest {
            caller: foreign_record,
            operation: PeerOperation::Sync { cursor: 0 },
        };
        let envelope = protocol::seal_workload_payload(
            protocol::WorkloadEnvelopeParts {
                nonce: &crypto::generate_secure_nonce(),
                now_millis: (now + 3) * 1000,
                sender_id: &foreign_endpoint.id().to_string(),
                recipient_id: &connection.remote_id().to_string(),
                sender_signing_public: foreign_node.signing_public(),
                sender_signing_private: foreign_node.signing_private(),
                sender_kem_public: None,
            },
            &request,
        )?;
        let (mut send, mut recv) = connection.open_bi().await?;
        protocol::write_workload_frame(
            &mut send,
            PeerRequest::TYPE.frame_kind(),
            &envelope,
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await?;
        assert!(
            protocol::read_workload_frame(
                &mut recv,
                Duration::from_secs(1),
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        connection.close(0u8.into(), b"foreign tenant check complete");
        foreign_endpoint.close().await;
        assert_eq!(caches[0].lock().unwrap().len(), 3);
        dns.shutdown().await?;
        for peer in peers.into_iter().flatten() {
            peer.shutdown().await?;
        }
        for app in apps {
            app.abort();
            let _ = app.await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
use tokio_util::sync::CancellationToken;
