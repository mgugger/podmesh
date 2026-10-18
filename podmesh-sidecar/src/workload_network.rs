//! Production workload-local DNS/peer lifecycle. Runtime control only installs
//! owner-authorized seeds; steady state refreshes and synchronizes over Iroh.
use crate::{
    authorization::WorkloadAuthorization,
    dns::{DnsForwarder, DnsLimits, LocalDns},
    egress_proxy::TunnelRequest,
    peer_connector::{PeerConnector, system_clock},
    service_cache::{ServiceCache, atomic_state},
};
use anyhow::{Context, Result, ensure};
use protocol::service_discovery::{ReplicaServiceRecord, SERVICE_RECORD_VERSION, ServiceCatalog};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_util::sync::CancellationToken;

pub(crate) struct WorkloadNetwork {
    pub cache: Arc<Mutex<ServiceCache>>,
    own: Mutex<Option<ReplicaServiceRecord>>,
    hints: Mutex<Vec<ReplicaServiceRecord>>,
    peer: tokio::sync::Mutex<Option<Arc<PeerConnector>>>,
    sequence: Mutex<(String, u64)>,
    endpoint: iroh::Endpoint,
    identity: Arc<iroh_support::NodeIdentity>,
    authorization: Arc<WorkloadAuthorization>,
    catalog: Option<ServiceCatalog>,
    replica: u32,
    directory: Option<PathBuf>,
    shutdown: CancellationToken,
}
impl WorkloadNetwork {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner: String,
        catalog: Option<ServiceCatalog>,
        replica: u32,
        endpoint: iroh::Endpoint,
        identity: Arc<iroh_support::NodeIdentity>,
        authorization: Arc<WorkloadAuthorization>,
        directory: Option<PathBuf>,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        let mut cache = match &directory {
            Some(dir) => ServiceCache::restore(owner.clone(), &dir.join("dns-state.json"))?,
            None => ServiceCache::new(owner.clone())?,
        };
        if let Some(catalog) = &catalog {
            cache.replace_catalog(catalog.clone())?;
        }
        let mut sequence = (String::new(), 0);
        let mut hints = Vec::new();
        if let Some(dir) = &directory {
            if let Some(bytes) = read_state(&dir.join("discovery-sequence.json"), 1024)? {
                sequence = serde_json::from_slice(&bytes)?;
            }
            if let Some(bytes) = read_state(&dir.join("peer-seeds.json"), 128 * 1024)? {
                let stored: Vec<ReplicaServiceRecord> = serde_json::from_slice(&bytes)?;
                ensure!(stored.len() <= 4, "too many stored peer seeds");
                for record in stored {
                    record.verify_structure(&owner)?;
                    hints.push(record);
                }
            }
        }
        let network = Arc::new(Self {
            cache: Arc::new(Mutex::new(cache)),
            own: Mutex::new(None),
            hints: Mutex::new(hints),
            peer: tokio::sync::Mutex::new(None),
            sequence: Mutex::new(sequence),
            endpoint,
            identity,
            authorization,
            catalog,
            replica,
            directory,
            shutdown,
        });
        network.persist()?;
        Ok(network)
    }
    pub fn record(&self) -> Result<Option<ReplicaServiceRecord>> {
        let now = system_clock()();
        Ok(self
            .own
            .lock()
            .map_err(|_| anyhow::anyhow!("network state poisoned"))?
            .clone()
            .filter(|record| record.verify(&record.catalog.owner, now).is_ok()))
    }
    pub fn seed(&self, record: ReplicaServiceRecord) -> Result<()> {
        let now = system_clock()();
        self.cache
            .lock()
            .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
            .seed(record.clone(), now)?;
        let mut hints = self
            .hints
            .lock()
            .map_err(|_| anyhow::anyhow!("seed state poisoned"))?;
        hints.retain(|old| {
            old.credential.delegation.scope.workload_id
                != record.credential.delegation.scope.workload_id
        });
        if hints.len() >= 4 {
            hints.remove(0);
        }
        hints.push(record);
        drop(hints);
        self.persist()
    }
    fn persist(&self) -> Result<()> {
        if let Some(dir) = &self.directory {
            self.cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
                .save(&dir.join("dns-state.json"))?;
            let hints = self
                .hints
                .lock()
                .map_err(|_| anyhow::anyhow!("seed state poisoned"))?
                .clone();
            atomic_state(&dir.join("peer-seeds.json"), &serde_json::to_vec(&hints)?)?;
        }
        Ok(())
    }
    pub async fn refresh(&self) -> Result<()> {
        let Some(catalog) = &self.catalog else {
            return Ok(());
        };
        let Some(credential) = self.authorization.current() else {
            return Ok(());
        };
        let now = system_clock()();
        credential.verify(Some(now))?;
        let digest = credential.delegation.digest()?;
        let sequence = {
            let mut state = self
                .sequence
                .lock()
                .map_err(|_| anyhow::anyhow!("sequence state poisoned"))?;
            if state.0 != digest {
                *state = (digest, 0);
            }
            state.1 = state
                .1
                .checked_add(1)
                .context("discovery sequence exhausted")?;
            if let Some(dir) = &self.directory {
                atomic_state(
                    &dir.join("discovery-sequence.json"),
                    &serde_json::to_vec(&*state)?,
                )?;
            }
            state.1
        };
        let address = self.endpoint.addr();
        let end = now
            .saturating_add(120)
            .min(credential.claims.expires_at_secs);
        ensure!(
            end > now,
            "workload certificate expires too soon for discovery"
        );
        let endpoint = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: self.endpoint.id().as_bytes().to_vec(),
            relay_url: address.relay_urls().next().map(ToString::to_string),
            direct_addresses: address
                .ip_addrs()
                .take(protocol::MAX_ENDPOINT_DIRECT_ADDRESSES)
                .map(ToString::to_string)
                .collect(),
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: end,
            signature: String::new(),
        }
        .sign(
            self.identity.signing_public(),
            self.identity.signing_private(),
            now,
        )?;
        let record = ReplicaServiceRecord {
            version: SERVICE_RECORD_VERSION,
            catalog: catalog.clone(),
            credential,
            endpoint,
            replica_index: self.replica,
            sequence,
            issued_at_secs: now,
            expires_at_secs: end,
            signature: String::new(),
        }
        .sign(self.identity.signing_private(), now)?;
        let peer = {
            let mut peer = self.peer.lock().await;
            match peer.as_ref() {
                Some(peer) => {
                    peer.update_record(record.clone())?;
                    peer.clone()
                }
                None => {
                    let created = Arc::new(PeerConnector::start(
                        self.endpoint.clone(),
                        self.identity.clone(),
                        record.clone(),
                        self.cache.clone(),
                        system_clock(),
                        std::net::Ipv4Addr::LOCALHOST,
                    )?);
                    *peer = Some(created.clone());
                    created
                }
            }
        };
        *self
            .own
            .lock()
            .map_err(|_| anyhow::anyhow!("network state poisoned"))? = Some(record);
        let mut neighbors = self
            .hints
            .lock()
            .map_err(|_| anyhow::anyhow!("seed state poisoned"))?
            .clone();
        for record in self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
            .snapshot(now)
        {
            if record.endpoint.endpoint_id != self.endpoint.id().as_bytes()
                && !neighbors
                    .iter()
                    .any(|old| old.endpoint.endpoint_id == record.endpoint.endpoint_id)
                && neighbors.len() < 4
            {
                neighbors.push(record);
            }
        }
        use futures::StreamExt;
        futures::stream::iter(neighbors.clone())
            .for_each_concurrent(4, |record| {
                let peer = peer.clone();
                async move {
                    if let Err(error) = peer.sync_with(&record).await {
                        log::debug!("peer synchronization unavailable: {error}");
                    }
                }
            })
            .await;
        // Keep fresh verified hints for restart bootstrap; stale hints may dial
        // only for synchronization and never become eligible service backends.
        let fresh = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
            .snapshot(system_clock()());
        for record in &mut neighbors {
            if let Some(current) = fresh
                .iter()
                .find(|current| current.endpoint.endpoint_id == record.endpoint.endpoint_id)
            {
                *record = current.clone();
            }
        }
        // Owner-control seeds may arrive while synchronization is in flight.
        // Refresh the current list rather than overwrite it with an old snapshot.
        {
            let mut hints = self
                .hints
                .lock()
                .map_err(|_| anyhow::anyhow!("seed state poisoned"))?;
            for hint in hints.iter_mut() {
                if let Some(current) = fresh
                    .iter()
                    .find(|record| record.endpoint.endpoint_id == hint.endpoint.endpoint_id)
                {
                    *hint = current.clone();
                }
            }
            for record in neighbors {
                if hints.len() < 4
                    && !hints.iter().any(|hint| {
                        hint.credential.delegation.scope.workload_id
                            == record.credential.delegation.scope.workload_id
                    })
                {
                    hints.push(record);
                }
            }
        }
        self.persist()
    }
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut changed = self.authorization.subscribe();
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        let mut online_refreshed = false;
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ok(()),
                _ = self.endpoint.online(), if !online_refreshed => {
                    online_refreshed = true;
                },
                _ = interval.tick() => {},
                result = changed.changed() => { result.context("workload authority channel closed")?; },
            }
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ok(()),
                result = self.refresh() => { if let Err(error) = result { log::warn!("workload discovery refresh unavailable: {error}"); } },
            }
        }
    }
    pub fn is_internal_destination(&self, host: &str) -> bool {
        if let Ok(address) = host.parse::<std::net::Ipv4Addr>()
            && address.octets()[..3] == [198, 18, 0]
        {
            return true;
        }
        self.cache
            .lock()
            .is_ok_and(|cache| cache.classifies_internal(host))
    }
    pub async fn forward(&self, mut tunnel: TunnelRequest) -> Result<()> {
        let now = system_clock()();
        self.authorization.credential(now)?;
        let key = {
            let cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
            match tunnel.dest_host.parse::<std::net::Ipv4Addr>() {
                Ok(address) => cache
                    .service_for_address(address)
                    .context("unknown service VIP")?,
                Err(_) => {
                    cache
                        .lookup(&tunnel.dest_host, now)
                        .context("internal DNS is unavailable")?
                        .key
                }
            }
        };
        let peer = self
            .peer
            .lock()
            .await
            .clone()
            .context("workload peer enrollment is pending")?;
        let remote = peer.open_service(&key, tunnel.dest_port, true).await;
        let mut local = None;
        let mut remote = match remote {
            Ok(remote) => Some(remote),
            Err(error) => {
                let own = self.record()?.context("no eligible local replica")?;
                ensure!(own.catalog.manifest_id == key.manifest_id, "{error}");
                let target = self
                    .cache
                    .lock()
                    .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
                    .target_port(&key, tunnel.dest_port)
                    .context("service port is not authorized")?;
                local = Some(
                    tokio::time::timeout(
                        Duration::from_secs(3),
                        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, target)),
                    )
                    .await??,
                );
                None
            }
        };
        if tunnel.send_http_200 {
            tunnel
                .client_stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
        }
        if let Some(bytes) = tunnel.initial_data.take() {
            if let Some(remote) = &mut remote {
                remote.send.write_all(&bytes).await?;
            } else {
                local.as_mut().unwrap().write_all(&bytes).await?;
            }
        }
        let (read, write) = tunnel.client_stream.into_split();
        let limits = iroh_support::raw_relay::RawRelayLimits {
            max_bytes_per_direction: 512 * 1024 * 1024,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(3600),
            authority_interval: Duration::from_secs(30),
        };
        if let Some(remote) = &mut remote {
            let remote_authority = remote.remote_identity.clone();
            iroh_support::raw_relay::supervise_raw_relay(
                read,
                write,
                &mut remote.recv,
                &mut remote.send,
                limits,
                self.shutdown.clone(),
                || {
                    let now = system_clock()();
                    self.authorization.credential(now)?;
                    remote_authority.verify(Some(now))
                },
            )
            .await?;
        } else {
            let (peer_read, peer_write) = local.unwrap().into_split();
            iroh_support::raw_relay::supervise_raw_relay(
                read,
                write,
                peer_read,
                peer_write,
                limits,
                self.shutdown.clone(),
                || self.authorization.credential(system_clock()()).map(|_| ()),
            )
            .await?;
        }
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<()> {
        let peer = self.peer.lock().await.take();
        if let Some(peer) = peer {
            peer.shutdown().await?;
        }
        Ok(())
    }
}

fn read_state(path: &std::path::Path, limit: usize) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "workload network state exceeds limit");
    Ok(Some(bytes))
}

pub(crate) async fn bind_dns(
    network: &Arc<WorkloadNetwork>,
    intercept: bool,
) -> Result<Vec<LocalDns>> {
    let forwarder = DnsForwarder::from_system()?;
    let forwarder = if intercept {
        forwarder.with_bypass_mark(crate::egress_nft::DNS_BYPASS_MARK)
    } else {
        forwarder
    };
    let forwarder = Arc::new(forwarder);
    let port = crate::egress_nft::DNS_PORT;
    let dns = LocalDns::bind(
        (std::net::Ipv4Addr::LOCALHOST, port).into(),
        network.cache.clone(),
        forwarder.clone(),
        system_clock(),
        DnsLimits::default(),
    )
    .await?;
    let mut servers = vec![dns];
    match LocalDns::bind(
        (std::net::Ipv6Addr::LOCALHOST, port).into(),
        network.cache.clone(),
        forwarder,
        system_clock(),
        DnsLimits::default(),
    )
    .await
    {
        Ok(dns) => servers.push(dns),
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                )
            }) => {}
        Err(error) => return Err(error),
    }
    Ok(servers)
}
