mod support;
use anyhow::Result;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::CNAME},
};
use podmesh_sidecar::{
    dns::{DnsForwarder, DnsLimits, LocalDns, MAX_DNS_PACKET_BYTES},
    service_cache::{ServiceCache, ServiceKey, is_internal_name, route_label},
};
use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

struct FakeDns {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    tcp_requests: Arc<AtomicUsize>,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}
impl FakeDns {
    async fn start(internal_alias: String) -> Result<Self> {
        let tcp = TcpListener::bind("127.0.0.1:0").await?;
        let address = tcp.local_addr()?;
        let udp = UdpSocket::bind(address).await?;
        let requests = Arc::new(AtomicUsize::new(0));
        let tcp_requests = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let udp_task = {
            let stop = stop.clone();
            let count = requests.clone();
            let alias = internal_alias.clone();
            tokio::spawn(async move {
                let mut bytes = vec![0; MAX_DNS_PACKET_BYTES];
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        result = udp.recv_from(&mut bytes) => {
                            let Ok((size, peer)) = result else { break; };
                            count.fetch_add(1, Ordering::SeqCst);
                            if let Ok(query) = Message::from_vec(&bytes[..size])
                                && let Some(response) = answer(query, false, &alias) {
                                let _ = udp.send_to(&response.to_vec().unwrap(), peer).await;
                            }
                        }
                    }
                }
            })
        };
        let tcp_task = {
            let stop = stop.clone();
            let count = tcp_requests.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        result = tcp.accept() => {
                            let Ok((mut stream, _)) = result else { break; };
                            let Ok(size) = stream.read_u16().await else { continue; };
                            let mut bytes = vec![0; size as usize];
                            if stream.read_exact(&mut bytes).await.is_err() { continue; }
                            count.fetch_add(1, Ordering::SeqCst);
                            if let Ok(query) = Message::from_vec(&bytes)
                                && let Some(response) = answer(query, true, &internal_alias) {
                                let bytes = response.to_vec().unwrap();
                                let _ = stream.write_u16(bytes.len() as u16).await;
                                let _ = stream.write_all(&bytes).await;
                            }
                        }
                    }
                }
            })
        };
        Ok(Self {
            address,
            requests,
            tcp_requests,
            stop,
            tasks: vec![udp_task, tcp_task],
        })
    }
    async fn shutdown(self) {
        self.stop.cancel();
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
    }
}
fn answer(query: Message, tcp: bool, internal_alias: &str) -> Option<Message> {
    let question = query.queries.first()?.clone();
    let name = question.name().to_string();
    if name == "timeout.test." {
        return None;
    }
    let mut response = Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
    response.queries.push(question.clone());
    response.metadata.recursion_available = true;
    if name == "bad-id.test." {
        response.metadata.id ^= 1;
    }
    if name == "missing.test." {
        response.metadata.response_code = ResponseCode::NXDomain;
        return Some(response);
    }
    if name == "fallback.test." && !tcp {
        response.metadata.truncation = true;
        return Some(response);
    }
    if name == "internal-alias.test." {
        response.answers.push(Record::from_rdata(
            question.name().clone(),
            60,
            RData::CNAME(CNAME(Name::from_ascii(internal_alias).unwrap())),
        ));
    } else if name == "alias.test." {
        response.answers.push(Record::from_rdata(
            question.name().clone(),
            60,
            RData::CNAME(CNAME(Name::from_ascii("public.test.").unwrap())),
        ));
        response.answers.push(Record::from_rdata(
            Name::from_ascii("public.test.").unwrap(),
            60,
            RData::A(Ipv4Addr::new(203, 0, 113, 9).into()),
        ));
    } else if question.query_type() == RecordType::AAAA {
        response.answers.push(Record::from_rdata(
            question.name().clone(),
            60,
            RData::AAAA("2001:db8::9".parse::<Ipv6Addr>().unwrap().into()),
        ));
    } else {
        response.answers.push(Record::from_rdata(
            question.name().clone(),
            60,
            RData::A(Ipv4Addr::new(203, 0, 113, 9).into()),
        ));
    }
    Some(response)
}
pub fn query(name: &str, kind: RecordType) -> Message {
    let mut message = Message::new(42, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message
        .queries
        .push(Query::query(Name::from_ascii(name).unwrap(), kind));
    message
}
async fn ask(address: SocketAddr, message: Message, tcp: bool) -> Result<Message> {
    let bytes = message.to_vec()?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let bytes = if tcp {
            let mut stream = TcpStream::connect(address).await?;
            stream.write_u16(bytes.len() as u16).await?;
            stream.write_all(&bytes).await?;
            let size = stream.read_u16().await? as usize;
            let mut bytes = vec![0; size];
            stream.read_exact(&mut bytes).await?;
            bytes
        } else {
            let socket = UdpSocket::bind("127.0.0.1:0").await?;
            socket.connect(address).await?;
            socket.send(&bytes).await?;
            let mut bytes = vec![0; MAX_DNS_PACKET_BYTES];
            let size = socket.recv(&mut bytes).await?;
            bytes.truncate(size);
            bytes
        };
        Ok::<_, anyhow::Error>(Message::from_vec(&bytes)?)
    })
    .await?
}

#[tokio::test]
async fn split_dns_is_local_private_and_preserves_public_forwarding() -> Result<()> {
    let authority = support::Authority::new();
    let catalog = authority.catalog(1234);
    let node = iroh_support::NodeIdentity::ephemeral();
    let now = support::now();
    let record = authority.record(&catalog, &node, vec!["127.0.0.1:1234".into()], 0, now, 10);
    let key = ServiceKey {
        manifest_id: catalog.manifest_id.clone(),
        name: "api".into(),
    };
    let fqdn = key.fqdn()?;
    let mut cache = ServiceCache::new(authority.owner.clone())?;
    cache.authorize_catalog(catalog.clone())?;
    cache.admit(record, now)?;
    let cache = Arc::new(Mutex::new(cache));
    let time = Arc::new(AtomicU64::new(now));
    let clock = {
        let time = time.clone();
        Arc::new(move || time.load(Ordering::SeqCst)) as podmesh_sidecar::peer_connector::Clock
    };
    let upstream = FakeDns::start(fqdn.clone()).await?;
    let forwarder = Arc::new(DnsForwarder::new(vec![upstream.address])?);
    let dns = LocalDns::bind(
        "127.0.0.1:0".parse()?,
        cache.clone(),
        forwarder,
        clock,
        DnsLimits::default(),
    )
    .await?;
    for tcp in [false, true] {
        let response = ask(
            dns.address(),
            query(&fqdn.to_uppercase(), RecordType::A),
            tcp,
        )
        .await?;
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert!(!response.metadata.authentic_data);
        assert!(matches!(response.answers[0].data, RData::A(_)));
        let ipv4 = match response.answers[0].data {
            RData::A(value) => value.0,
            _ => unreachable!(),
        };
        assert_eq!(ipv4.octets()[..2], [198, 18]);
        assert_eq!(response.metadata.id, 42);
        let ipv6 = ask(dns.address(), query(&fqdn, RecordType::AAAA), tcp).await?;
        assert_eq!(ipv6.metadata.response_code, ResponseCode::NoError);
        assert!(ipv6.answers.is_empty());
    }
    let unknown = ask(
        dns.address(),
        query("unknown.svc.podmesh.internal.", RecordType::A),
        false,
    )
    .await?;
    assert_eq!(unknown.metadata.response_code, ResponseCode::ServFail);
    assert_eq!(upstream.requests.load(Ordering::SeqCst), 0);
    let public = ask(dns.address(), query("public.test.", RecordType::A), false).await?;
    assert_eq!(public.metadata.response_code, ResponseCode::NoError);
    assert_eq!(public.answers.len(), 1);
    let calls = upstream.requests.load(Ordering::SeqCst);
    ask(dns.address(), query("public.test.", RecordType::A), true).await?;
    assert_eq!(
        upstream.requests.load(Ordering::SeqCst),
        calls,
        "forward answers should be cached"
    );
    assert!(matches!(
        ask(dns.address(), query("v6.test.", RecordType::AAAA), false)
            .await?
            .answers[0]
            .data,
        RData::AAAA(_)
    ));
    assert_eq!(
        ask(dns.address(), query("alias.test.", RecordType::A), false)
            .await?
            .answers
            .len(),
        2
    );
    let calls = upstream.requests.load(Ordering::SeqCst);
    let alias = ask(
        dns.address(),
        query("internal-alias.test.", RecordType::A),
        false,
    )
    .await?;
    assert!(matches!(alias.answers[0].data, RData::CNAME(_)));
    assert_eq!(
        upstream.requests.load(Ordering::SeqCst),
        calls + 1,
        "CNAME targets must not be chased publicly"
    );
    assert_eq!(
        ask(dns.address(), query("fallback.test.", RecordType::A), false)
            .await?
            .metadata
            .response_code,
        ResponseCode::NoError
    );
    assert_eq!(upstream.tcp_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        ask(dns.address(), query("missing.test.", RecordType::A), false)
            .await?
            .metadata
            .response_code,
        ResponseCode::NXDomain
    );
    assert_eq!(
        ask(dns.address(), query("timeout.test.", RecordType::A), false)
            .await?
            .metadata
            .response_code,
        ResponseCode::ServFail
    );
    assert_eq!(
        ask(dns.address(), query("bad-id.test.", RecordType::A), false)
            .await?
            .metadata
            .response_code,
        ResponseCode::ServFail
    );
    let before = cache
        .lock()
        .unwrap()
        .service_for_address(Ipv4Addr::new(198, 18, 0, 1));
    let calls = upstream.requests.load(Ordering::SeqCst);
    time.store(now + 11, Ordering::SeqCst);
    assert_eq!(
        ask(dns.address(), query(&fqdn, RecordType::A), false)
            .await?
            .metadata
            .response_code,
        ResponseCode::ServFail
    );
    assert_eq!(
        cache
            .lock()
            .unwrap()
            .service_for_address(Ipv4Addr::new(198, 18, 0, 1)),
        before
    );
    assert_eq!(upstream.requests.load(Ordering::SeqCst), calls);
    dns.shutdown().await?;
    upstream.shutdown().await;
    Ok(())
}

#[test]
fn service_cache_refuses_tampering_cross_tenant_replay_and_bad_names() -> Result<()> {
    let authority = support::Authority::new();
    let catalog = authority.catalog(1234);
    let now = support::now();
    let node = iroh_support::NodeIdentity::ephemeral();
    let record = authority.record(&catalog, &node, vec!["127.0.0.1:1234".into()], 0, now, 10);
    let mut cache = ServiceCache::new(authority.owner.clone())?;
    cache.authorize_catalog(catalog.clone())?;
    assert!(cache.admit(record.clone(), now)?);
    assert!(!cache.admit(record.clone(), now)?);
    let mut newer = record.clone();
    newer.sequence = 2;
    newer = newer.sign(node.signing_private(), now)?;
    cache.admit(newer.clone(), now)?;
    assert!(cache.admit(record.clone(), now).is_err());
    let mut conflict = newer;
    conflict.expires_at_secs -= 1;
    conflict = conflict.sign(node.signing_private(), now)?;
    assert!(cache.admit(conflict, now).is_err());
    let mut altered = record.clone();
    altered.catalog.services[0].ports[0].target_port = 22;
    assert!(cache.admit(altered, now).is_err());
    let mut altered = record.clone();
    altered.endpoint.endpoint_id = iroh::SecretKey::generate().public().as_bytes().to_vec();
    assert!(cache.admit(altered, now).is_err());
    let foreign = support::Authority::new();
    let foreign_catalog = foreign.catalog(1234);
    assert!(cache.authorize_catalog(foreign_catalog.clone()).is_err());
    assert!(
        cache
            .admit(
                foreign.record(
                    &foreign_catalog,
                    &node,
                    vec!["127.0.0.1:1234".into()],
                    0,
                    now,
                    10
                ),
                now
            )
            .is_err()
    );
    assert!(cache.admit(record, now + 11).is_err());
    assert_eq!(cache.len(), 1);
    assert_eq!(route_label(&catalog.manifest_id)?.len(), 52);
    assert!(route_label(&"é".repeat(32)).is_err());
    assert!(is_internal_name("X.SVC.PODMESH.INTERNAL"));
    assert!(!is_internal_name("x.svc.podmesh.internal.evil."));
    Ok(())
}

#[test]
fn anti_entropy_ignores_authenticated_old_snapshots_without_regressing_or_accepting_forgery()
-> Result<()> {
    let authority = support::Authority::new();
    let catalog = authority.catalog(1234);
    let now = support::now();
    let node = iroh_support::NodeIdentity::ephemeral();
    let old = authority.record(&catalog, &node, vec!["127.0.0.1:1234".into()], 0, now, 60);
    let mut newer = old.clone();
    newer.sequence += 1;
    let newer = newer.sign(node.signing_private(), now)?;
    let mut cache = ServiceCache::new(authority.owner.clone())?;
    cache.authorize_catalog(catalog)?;
    cache.admit(newer.clone(), now)?;
    assert!(cache.admit(old.clone(), now).is_err());
    assert!(!cache.merge(old.clone(), now)?);
    assert_eq!(cache.snapshot(now), vec![newer]);
    let mut forged = old;
    forged.sequence = 0;
    assert!(cache.merge(forged, now).is_err());
    Ok(())
}

#[test]
fn same_service_and_workload_name_is_not_an_ambiguous_alias() -> Result<()> {
    let authority = support::Authority::new();
    let catalog = authority.catalog_named(1234, "demo");
    let node = iroh_support::NodeIdentity::ephemeral();
    let now = support::now();
    let mut cache = ServiceCache::new(authority.owner.clone())?;
    cache.authorize_catalog(catalog.clone())?;
    cache.admit(
        authority.record(&catalog, &node, vec!["127.0.0.1:1234".into()], 0, now, 60),
        now,
    )?;
    assert!(cache.lookup("demo.", now).is_some());
    Ok(())
}

#[test]
fn persisted_vips_survive_restart_and_catalog_replace_without_reassignment() -> Result<()> {
    let authority = support::Authority::new();
    let catalog = authority.catalog(1234);
    let now = support::now();
    let node = iroh_support::NodeIdentity::ephemeral();
    let record = authority.record(&catalog, &node, vec!["127.0.0.1:1234".into()], 0, now, 60);
    let mut cache = ServiceCache::new(authority.owner.clone())?;
    cache.authorize_catalog(catalog.clone())?;
    cache.admit(record.clone(), now)?;
    let key = ServiceKey {
        manifest_id: catalog.manifest_id.clone(),
        name: "api".into(),
    };
    let before = cache.lookup(&key.fqdn()?, now).unwrap().address;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("dns.json");
    cache.save(&path)?;
    let mut restored = ServiceCache::restore(authority.owner.clone(), &path)?;
    assert_eq!(restored.service_for_address(before), Some(key.clone()));
    assert!(restored.lookup(&key.fqdn()?, now).is_none()); // records are soft state
    restored.admit(record, now)?;
    assert_eq!(restored.lookup("api.", now).unwrap().address, before);
    restored.replace_catalog(authority.catalog(4321))?;
    assert_eq!(restored.service_for_address(before), Some(key.clone()));
    assert_eq!(restored.target_port(&key, 80), Some(4321));
    assert!(ServiceCache::restore(support::Authority::new().owner, &path).is_err());
    Ok(())
}

#[tokio::test]
async fn dns_admission_deadlines_limits_and_shutdown_are_bounded() -> Result<()> {
    let authority = support::Authority::new();
    let cache = Arc::new(Mutex::new(ServiceCache::new(authority.owner)?));
    let upstream = FakeDns::start("unused.svc.podmesh.internal.".into()).await?;
    let forwarder = Arc::new(DnsForwarder::new(vec![upstream.address])?);
    assert!(
        LocalDns::bind(
            "0.0.0.0:0".parse()?,
            cache.clone(),
            forwarder.clone(),
            podmesh_sidecar::peer_connector::system_clock(),
            DnsLimits::default()
        )
        .await
        .is_err()
    );
    let dns = LocalDns::bind(
        "127.0.0.1:0".parse()?,
        cache,
        forwarder,
        podmesh_sidecar::peer_connector::system_clock(),
        DnsLimits {
            max_handlers: 1,
            request_timeout: Duration::from_millis(150),
        },
    )
    .await?;
    let refused = ask(
        dns.address(),
        query("public.test.", RecordType::AXFR),
        false,
    )
    .await?;
    assert_eq!(refused.metadata.response_code, ResponseCode::Refused);
    let mut update = query("public.test.", RecordType::A);
    update.metadata.op_code = OpCode::Update;
    assert_eq!(
        ask(dns.address(), update, false)
            .await?
            .metadata
            .response_code,
        ResponseCode::Refused
    );
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket
        .send_to(&vec![0; MAX_DNS_PACKET_BYTES + 1], dns.address())
        .await?;
    let mut bytes = [0; 512];
    assert!(
        tokio::time::timeout(Duration::from_millis(30), socket.recv(&mut bytes))
            .await
            .is_err()
    );
    let mut slow = TcpStream::connect(dns.address()).await?;
    slow.write_u16(100).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_connection_closed(slow.read(&mut bytes).await);
    assert_eq!(
        ask(dns.address(), query("public.test.", RecordType::A), true)
            .await?
            .metadata
            .response_code,
        ResponseCode::NoError
    );
    let mut oversized = TcpStream::connect(dns.address()).await?;
    oversized
        .write_u16((MAX_DNS_PACKET_BYTES + 1) as u16)
        .await?;
    assert_connection_closed(
        tokio::time::timeout(Duration::from_secs(1), oversized.read(&mut bytes)).await?,
    );
    let mut partial = TcpStream::connect(dns.address()).await?;
    partial.write_u16(100).await?;
    dns.shutdown().await?;
    assert_connection_closed(
        tokio::time::timeout(Duration::from_secs(1), partial.read(&mut bytes)).await?,
    );
    upstream.shutdown().await;
    Ok(())
}

fn assert_connection_closed(result: std::io::Result<usize>) {
    match result {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) => {}
        other => panic!("connection was not closed: {other:?}"),
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("proptest-regressions/local_dns.txt"))),
        ..proptest::test_runner::Config::default()
    })]
    #[test]
    fn full_route_digests_fit_dns_labels(bytes in proptest::arbitrary::any::<[u8; 32]>()) {
        let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let label = route_label(&hex).unwrap();
        proptest::prop_assert_eq!(label.len(), 52);
        proptest::prop_assert_eq!(data_encoding::BASE32_NOPAD.decode(label.to_uppercase().as_bytes()).unwrap(), bytes.to_vec());
    }
}
