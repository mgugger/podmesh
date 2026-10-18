//! Production sidecar startup/control/DNS/CONNECT path, without kernel rules
//! or containers. A remote application is unreachable on local loopback, so
//! success proves real peer routing rather than accidental local fallback.
mod support;
use anyhow::{Context, Result, ensure};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query},
    rr::{Name, RData, RecordType},
};
use podmesh_sidecar::{
    IdentitySource, SidecarConfig,
    peer_connector::{PeerConnector, system_clock},
    run_sidecar_with_shutdown,
    service_cache::{ServiceCache, ServiceKey},
};
use protocol::workload_connector::ConnectorRequest;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_sidecar_serves_dns_accepts_seed_and_routes_internal_tcp_without_proxy()
-> Result<()> {
    for relay_configured in [false, true] {
        production_path(relay_configured).await?;
    }
    Ok(())
}

async fn production_path(relay_configured: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(25), async {
        let authority = support::Authority::new();
        let now = support::now();
        let app = TcpListener::bind("127.0.0.2:0").await?;
        let app_port = app.local_addr()?.port();
        let catalog = authority.catalog(app_port);
        let app_task = tokio::spawn(async move {
            let (mut connection, _) = app.accept().await.unwrap();
            let mut bytes = [0; 5];
            connection.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"hello");
            connection.write_all(b"remote-reply").await.unwrap();
            connection.shutdown().await.unwrap();
        });
        let remote_node = Arc::new(iroh_support::NodeIdentity::ephemeral());
        let remote_endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(remote_node.transport_secret().clone())
            .bind_addr("127.0.0.1:0")?
            .clear_relay_transports()
            .bind()
            .await?;
        let remote_record = authority.record(
            &catalog,
            &remote_node,
            remote_endpoint
                .addr()
                .ip_addrs()
                .map(ToString::to_string)
                .collect(),
            1,
            now,
            120,
        );
        let remote = PeerConnector::start(
            remote_endpoint,
            remote_node,
            remote_record.clone(),
            Arc::new(Mutex::new(ServiceCache::new(authority.owner.clone())?)),
            system_clock(),
            "127.0.0.2".parse()?,
        )?;
        let directory = tempfile::tempdir()?;
        let source = IdentitySource::Persistent(directory.path().join("identity"));
        let own = source.load()?;
        let own_record =
            authority.record(&catalog, &own, vec!["127.0.0.1:1234".into()], 0, now, 120);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
        let connect_port = proxy_listener.local_addr()?.port();
        drop(proxy_listener);
        let (public, private) = crypto::generate_signing_keypair();
        let absent_proxy = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: iroh::SecretKey::generate().public().as_bytes().to_vec(),
            relay_url: relay_configured.then(|| "http://127.0.0.1:9".into()),
            direct_addresses: vec!["127.0.0.1:9".into()],
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now + 600,
            signature: String::new(),
        }
        .sign(&public, &private, now)?;
        let socket = directory.path().join("control/control.sock");
        let config = SidecarConfig {
            identity: source,
            connector_socket: Some(socket.clone()),
            service_catalog: Some(catalog.clone()),
            proxy_endpoints: vec![absent_proxy],
            workload_replay_limits: protocol::ReplayLimits::default(),
            workload_relay_auth_token: relay_configured.then(|| "r".repeat(32)),
            workload_relay_ca_certificates: Vec::new(),
            lookup_interval: Duration::from_secs(1),
            iroh_bind_addr: "127.0.0.1:0".parse()?,
            metrics_listen: None,
            workload_name: "demo".into(),
            manifest_id: catalog.manifest_id.clone(),
            replica_index: 0,
            replica_count: 2,
            ingress_host: "unused.mesh.local".into(),
            app_port: 18080,
            routes: Vec::new(),
            owner_public_key_b64: Some(authority.owner.clone()),
            workload_credential_b64: Some(own_record.credential.to_b64()?),
            enable_egress: false,
            skip_egress_nft: true,
            http_proxy_port: Some(connect_port),
        };
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run_sidecar_with_shutdown(config, stopped, None));
        let result: Result<()> = async {
            let report = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(response) = podmesh_sidecar::connector_control::exchange(
                        &socket,
                        &ConnectorRequest::inspect([1; 32]),
                    )
                    .await
                        && let Some(report) = response.identity
                        && report.service_record.is_some() != relay_configured
                    {
                        return Ok::<_, anyhow::Error>(report);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await??;
            ensure!(
                report.credential.is_some(),
                "sidecar did not publish bound authority"
            );
            let response = podmesh_sidecar::connector_control::exchange(
                &socket,
                &ConnectorRequest::seed([2; 32], remote_record),
            )
            .await?;
            ensure!(
                response.error.is_none(),
                "owner-authorized peer seed was refused"
            );
            let key = ServiceKey {
                manifest_id: catalog.manifest_id.clone(),
                name: "api".into(),
            };
            let name = key.fqdn()?;
            let mut query = Message::new(52, MessageType::Query, OpCode::Query);
            query.metadata.recursion_desired = true;
            query
                .queries
                .push(Query::query(Name::from_ascii(&name)?, RecordType::A));
            let dns = UdpSocket::bind("127.0.0.1:0").await?;
            dns.connect("127.0.0.1:15053").await?;
            dns.send(&query.to_vec()?).await?;
            let mut bytes = vec![0; 4096];
            let size = dns.recv(&mut bytes).await?;
            let answer = Message::from_vec(&bytes[..size])?;
            let RData::A(address) = answer
                .answers
                .first()
                .context("production DNS answer missing")?
                .data
            else {
                anyhow::bail!("production DNS returned no service VIP");
            };
            assert_eq!(address.0.octets()[..2], [198, 18]);
            let mut stream =
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, connect_port)).await?;
            stream
                .write_all(
                    format!("CONNECT {name}:80 HTTP/1.1\r\nHost: {name}:80\r\n\r\n").as_bytes(),
                )
                .await?;
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await?;
                header.push(byte[0]);
                ensure!(header.len() < 4096, "CONNECT header exceeded bound");
            }
            ensure!(
                header.starts_with(b"HTTP/1.1 200"),
                "internal CONNECT failed"
            );
            stream.write_all(b"hello").await?;
            let mut reply = [0; 12];
            stream.read_exact(&mut reply).await?;
            assert_eq!(&reply, b"remote-reply");
            ensure!(
                directory.path().join("identity/dns-state.json").exists(),
                "DNS mapping was not persisted"
            );
            Ok(())
        }
        .await;
        let _ = stop.send(());
        let stopped = task.await?;
        remote.shutdown().await?;
        app_task.abort();
        let _ = app_task.await;
        stopped?;
        result
    })
    .await?
}
