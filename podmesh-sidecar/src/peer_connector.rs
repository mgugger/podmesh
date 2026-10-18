//! Explicit peer connector API for container-free discovery/DNS development.
//! Uses the workload's Iroh keys, owner-authorized catalogs and bound credentials.
//! No proxy, scheduler, host networking mutation or privileged listener is needed.
use crate::service_cache::{ServiceCache, ServiceKey};
use anyhow::{Context, Result, ensure};
use iroh::{
    Endpoint,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_support::NodeIdentity;
use protocol::{
    WorkloadPayload,
    peer_control::{
        MAX_PEER_RECORD_BATCH, PEER_ALPN, PeerOperation, PeerRequest, PeerResponse, PeerResult,
    },
    service_discovery::{ReplicaServiceRecord, endpoint_hex},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpStream, sync::Semaphore};
use tokio_util::sync::CancellationToken;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    })
}
pub const PEER_TIMEOUT: Duration = Duration::from_secs(3);
const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PEER_SESSIONS: usize = 32;
const MAX_SYNC_PAGES: usize = 256; // fixed cache ceiling / batch size

struct PeerState {
    endpoint: Endpoint,
    identity: Arc<NodeIdentity>,
    own: Mutex<ReplicaServiceRecord>,
    cache: Arc<Mutex<ServiceCache>>,
    clock: Clock,
    replay: protocol::PeerReplayRegistry,
    slots: Arc<Semaphore>,
    incoming_slots: Arc<Semaphore>,
    connections: Mutex<std::collections::BTreeMap<iroh::EndpointId, Connection>>,
    shutdown: CancellationToken,
    application_host: std::net::Ipv4Addr,
}
impl std::fmt::Debug for PeerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerState").finish_non_exhaustive()
    }
}

pub struct PeerConnector {
    state: Arc<PeerState>,
    router: Mutex<Option<Router>>,
}

pub struct PeerTunnel {
    pub send: iroh::endpoint::SendStream,
    pub recv: iroh::endpoint::RecvStream,
    pub remote_workload_id: String,
    _connection: Connection,
    _permit: tokio::sync::OwnedSemaphorePermit,
    pub(crate) remote_identity: protocol::workload_identity::WorkloadIdentity,
}
impl PeerTunnel {
    pub fn verify_authority(&self, now: u64) -> Result<()> {
        self.remote_identity.verify(Some(now))
    }
}
impl Drop for PeerTunnel {
    fn drop(&mut self) {
        let _ = self.send.finish();
        let _ = self.recv.stop(0u8.into());
    }
}

impl PeerConnector {
    pub fn start(
        endpoint: Endpoint,
        identity: Arc<NodeIdentity>,
        own: ReplicaServiceRecord,
        cache: Arc<Mutex<ServiceCache>>,
        clock: Clock,
        application_host: std::net::Ipv4Addr,
    ) -> Result<Self> {
        ensure!(
            application_host.is_loopback(),
            "peer application target must be loopback"
        );
        ensure!(
            endpoint.id() == identity.endpoint_id()
                && endpoint_hex(&own.endpoint) == endpoint.id().to_string(),
            "peer endpoint identity mismatch"
        );
        ensure!(
            own.endpoint.signing_pubkey == crypto::b64_encode(identity.signing_public()),
            "peer application identity mismatch"
        );
        {
            let mut view = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
            own.verify(view.owner(), clock())?;
            view.authorize_catalog(own.catalog.clone())?;
            view.admit(own.clone(), clock())?;
        }
        let replay = protocol::PeerReplayRegistry::new(protocol::ReplayLimits {
            max_peers: 64,
            nonces_per_peer: 64,
            ..protocol::ReplayLimits::default()
        })?;
        let state = Arc::new(PeerState {
            endpoint: endpoint.clone(),
            identity,
            own: Mutex::new(own),
            cache,
            clock,
            replay,
            slots: Arc::new(Semaphore::new(MAX_PEER_SESSIONS)),
            incoming_slots: Arc::new(Semaphore::new(MAX_PEER_SESSIONS)),
            connections: Mutex::new(std::collections::BTreeMap::new()),
            shutdown: CancellationToken::new(),
            application_host,
        });
        let router = Router::builder(endpoint)
            .accept(PEER_ALPN, state.clone())
            .spawn();
        Ok(Self {
            state,
            router: Mutex::new(Some(router)),
        })
    }
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.state.endpoint.id()
    }
    pub fn record(&self) -> Result<ReplicaServiceRecord> {
        self.state.own_record()
    }
    pub fn update_record(&self, record: ReplicaServiceRecord) -> Result<()> {
        let mut own = self
            .state
            .own
            .lock()
            .map_err(|_| anyhow::anyhow!("peer state poisoned"))?;
        ensure!(
            record.endpoint.endpoint_id == own.endpoint.endpoint_id
                && record.endpoint.signing_pubkey == own.endpoint.signing_pubkey
                && record.catalog == own.catalog
                && record.credential.delegation == own.credential.delegation,
            "peer renewal changes identity/catalog"
        );
        self.state
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
            .admit(record.clone(), (self.state.clock)())?;
        *own = record;
        Ok(())
    }
    /// A bounded paginated anti-entropy exchange. Call periodically through a
    /// bounded neighbor set; initial deployment seed injection remains separate.
    pub async fn sync_with(&self, peer: &ReplicaServiceRecord) -> Result<usize> {
        tokio::time::timeout(Duration::from_secs(10), self.sync_inner(peer))
            .await
            .context("peer synchronization timed out")?
    }
    async fn sync_inner(&self, peer: &ReplicaServiceRecord) -> Result<usize> {
        let _permit = self
            .state
            .slots
            .clone()
            .try_acquire_owned()
            .context("peer sync capacity reached")?;
        let mut cursor = 0;
        let mut changed = 0;
        for _ in 0..MAX_SYNC_PAGES {
            let (_, mut send, mut recv, response) =
                self.exchange(peer, PeerOperation::Sync { cursor }).await?;
            let _ = send.finish();
            let _ = recv.stop(0u8.into());
            let PeerResult::Records { records, next } = response.result else {
                anyhow::bail!("peer sync refused");
            };
            let mut cache = self
                .state
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
            for record in records {
                if cache.authorize_catalog(record.catalog.clone()).is_err() {
                    continue;
                }
                if let Ok(merged) = cache.merge(record, (self.state.clock)()) {
                    changed += usize::from(merged);
                }
            }
            drop(cache);
            match next {
                Some(next) => {
                    ensure!(next > cursor, "peer pagination did not advance");
                    cursor = next;
                }
                None => return Ok(changed),
            }
        }
        anyhow::bail!("peer synchronization exceeded page bound")
    }

    pub async fn open_service(
        &self,
        key: &ServiceKey,
        port: u16,
        remote_only: bool,
    ) -> Result<PeerTunnel> {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.open_service_inner(key, port, remote_only),
        )
        .await
        .context("service peer setup deadline exceeded")?
    }
    async fn open_service_inner(
        &self,
        key: &ServiceKey,
        port: u16,
        remote_only: bool,
    ) -> Result<PeerTunnel> {
        let permit = self
            .state
            .slots
            .clone()
            .try_acquire_owned()
            .context("peer connection capacity reached")?;
        let own_endpoint = self.endpoint_id();
        let peers = {
            let mut cache = self
                .state
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
            ensure!(
                cache.target_port(key, port).is_some(),
                "unknown service port"
            );
            cache.candidates(
                key,
                (self.state.clock)(),
                remote_only.then_some(own_endpoint.as_bytes().as_slice()),
            )
        };
        for peer in peers {
            let operation = PeerOperation::Connect {
                manifest_id: key.manifest_id.clone(),
                service: key.name.clone(),
                port,
            };
            let exchange = self.exchange(&peer, operation).await;
            if let Err(error) = &exchange {
                log::warn!("internal peer setup failed: {error:#}");
            }
            if let Ok((connection, send, recv, response)) = exchange {
                if matches!(response.result, PeerResult::Connected) {
                    return Ok(PeerTunnel {
                        send,
                        recv,
                        remote_workload_id: response
                            .responder
                            .credential
                            .delegation
                            .scope
                            .workload_id
                            .clone(),
                        remote_identity: response.responder.credential,
                        _connection: connection,
                        _permit: permit,
                    });
                }
                // A refused service stream must not tear down concurrent
                // control/application streams on the same authenticated peer.
                drop((send, recv));
            }
        }
        anyhow::bail!("no reachable authorized service replica")
    }
    pub async fn open_address(
        &self,
        address: std::net::Ipv4Addr,
        port: u16,
        remote_only: bool,
    ) -> Result<PeerTunnel> {
        let key = self
            .state
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
            .service_for_address(address)
            .context("unknown synthetic service address")?;
        self.open_service(&key, port, remote_only).await
    }

    async fn exchange(
        &self,
        peer: &ReplicaServiceRecord,
        operation: PeerOperation,
    ) -> Result<(
        Connection,
        iroh::endpoint::SendStream,
        iroh::endpoint::RecvStream,
        PeerResponse,
    )> {
        let owner = self.state.owner()?;
        let address = if matches!(operation, PeerOperation::Sync { .. }) {
            peer.verify_structure(&owner)?;
            iroh_support::endpoint_addr_hint(&peer.endpoint)?
        } else {
            peer.verify(&owner, (self.state.clock)())?;
            iroh_support::endpoint_addr(&peer.endpoint, (self.state.clock)())?
        };
        let cached = self
            .state
            .connections
            .lock()
            .map_err(|_| anyhow::anyhow!("peer connections poisoned"))?
            .get(&address.id)
            .filter(|connection| connection.close_reason().is_none())
            .cloned();
        let connection = if let Some(connection) = cached {
            connection
        } else {
            let connection = tokio::time::timeout(
                PEER_CONNECT_TIMEOUT,
                self.state.endpoint.connect(address, PEER_ALPN),
            )
            .await
            .context("peer QUIC dial timed out")??;
            let mut connections = self
                .state
                .connections
                .lock()
                .map_err(|_| anyhow::anyhow!("peer connections poisoned"))?;
            connections.retain(|_, connection| connection.close_reason().is_none());
            if let Some(existing) = connections.get(&connection.remote_id()) {
                let existing = existing.clone();
                connection.close(0u8.into(), b"duplicate peer connection");
                existing
            } else {
                if connections.len() >= MAX_PEER_SESSIONS {
                    connection.close(1u8.into(), b"peer connection capacity reached");
                    anyhow::bail!("peer connection cache full");
                }
                connections.insert(connection.remote_id(), connection.clone());
                connection
            }
        };
        let result = async {
            let request = PeerRequest {
                caller: self.state.own_record()?,
                operation,
            };
            let bytes = self.state.seal(&connection, &request)?;
            let (mut send, mut recv) = connection.open_bi().await?;
            protocol::write_workload_frame(
                &mut send,
                PeerRequest::TYPE.frame_kind(),
                &bytes,
                PEER_TIMEOUT,
                &self.state.shutdown,
            )
            .await?;
            let (kind, bytes) =
                protocol::read_workload_frame(&mut recv, PEER_TIMEOUT, &self.state.shutdown)
                    .await?;
            ensure!(
                kind == PeerResponse::TYPE.frame_kind(),
                "unexpected peer response frame"
            );
            let accepted = self
                .state
                .accept_payload::<PeerResponse>(&connection, &bytes)?;
            self.state.verify_remote(
                &connection,
                &accepted.payload.responder,
                &accepted.sender_signing_key,
            )?;
            let response = accepted.payload;
            {
                let mut cache = self
                    .state
                    .cache
                    .lock()
                    .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
                cache.authorize_catalog(response.responder.catalog.clone())?;
                cache.merge(response.responder.clone(), (self.state.clock)())?;
            }
            Ok::<_, anyhow::Error>((send, recv, response))
        };
        match tokio::time::timeout(PEER_TIMEOUT, result).await {
            Ok(Ok((send, recv, response))) => Ok((connection, send, recv, response)),
            result => {
                // The cached transport may carry other service/sync streams.
                // Dropping this operation's streams bounds its failure; closing
                // the connection here would reset unrelated authorized traffic.
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(_) => anyhow::bail!("peer setup timed out"),
                    _ => unreachable!(),
                }
            }
        }
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.state.shutdown.cancel();
        let router = self
            .router
            .lock()
            .map_err(|_| anyhow::anyhow!("peer router poisoned"))?
            .take();
        if let Some(router) = router {
            router.shutdown().await?;
        }
        self.state.endpoint.close().await;
        Ok(())
    }
}

impl PeerState {
    fn owner(&self) -> Result<String> {
        Ok(self.own_record()?.catalog.owner)
    }
    fn own_record(&self) -> Result<ReplicaServiceRecord> {
        Ok(self
            .own
            .lock()
            .map_err(|_| anyhow::anyhow!("peer state poisoned"))?
            .clone())
    }
    fn seal<T: WorkloadPayload>(&self, connection: &Connection, value: &T) -> Result<Vec<u8>> {
        protocol::seal_workload_payload(
            protocol::WorkloadEnvelopeParts {
                nonce: &crypto::generate_secure_nonce(),
                now_millis: (self.clock)().saturating_mul(1000),
                sender_id: &self.endpoint.id().to_string(),
                recipient_id: &connection.remote_id().to_string(),
                sender_signing_public: self.identity.signing_public(),
                sender_signing_private: self.identity.signing_private(),
                sender_kem_public: None,
            },
            value,
        )
    }
    fn accept_payload<T: WorkloadPayload>(
        &self,
        connection: &Connection,
        bytes: &[u8],
    ) -> Result<protocol::AcceptedWorkloadPayload<T>> {
        protocol::accept_workload_payload(
            bytes,
            &self.endpoint.id().to_string(),
            &connection.remote_id().to_string(),
            (self.clock)().saturating_mul(1000),
            &self.replay,
            Instant::now(),
        )
    }
    fn verify_remote(
        &self,
        connection: &Connection,
        record: &ReplicaServiceRecord,
        signer: &[u8],
    ) -> Result<()> {
        record.verify(&self.owner()?, (self.clock)())?;
        ensure!(
            endpoint_hex(&record.endpoint) == connection.remote_id().to_string(),
            "peer record does not match transport"
        );
        ensure!(
            record.endpoint.signing_pubkey == crypto::b64_encode(signer),
            "peer envelope signer is not authorized"
        );
        Ok(())
    }
    async fn serve(&self, connection: Connection) -> Result<()> {
        use futures::{StreamExt, stream::FuturesUnordered};
        let _session = self
            .incoming_slots
            .clone()
            .try_acquire_owned()
            .context("peer session capacity reached")?;
        let mut operations = FuturesUnordered::new();
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ok(()),
                Some(result) = operations.next(), if !operations.is_empty() => {
                    if let Err(error) = result { log::debug!("peer stream refused: {error:#}"); }
                },
                incoming = tokio::time::timeout(Duration::from_secs(60), connection.accept_bi()), if operations.len() < MAX_PEER_SESSIONS => {
                    match incoming {
                        Ok(Ok((send, recv))) => operations.push(self.serve_stream(connection.clone(), send, recv)),
                        Ok(Err(_)) => return Ok(()),
                        Err(_) if operations.is_empty() => return Ok(()),
                        Err(_) => {},
                    }
                }
            }
        }
    }
    async fn serve_stream(
        &self,
        connection: Connection,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<()> {
        let _permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .context("peer handler capacity reached")?;
        let (kind, bytes) =
            protocol::read_workload_frame(&mut recv, PEER_TIMEOUT, &self.shutdown).await?;
        ensure!(
            kind == PeerRequest::TYPE.frame_kind(),
            "unexpected peer request frame"
        );
        let accepted = self.accept_payload::<PeerRequest>(&connection, &bytes)?;
        self.verify_remote(
            &connection,
            &accepted.payload.caller,
            &accepted.sender_signing_key,
        )?;
        let caller = accepted.payload.caller;
        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
            cache.authorize_catalog(caller.catalog.clone())?;
            cache.merge(caller.clone(), (self.clock)())?;
        }
        let own = self.own_record()?;
        let mut application = None;
        let result = match accepted.payload.operation {
            PeerOperation::Sync { cursor } => {
                let records = self
                    .cache
                    .lock()
                    .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
                    .snapshot((self.clock)());
                let start = usize::from(cursor).min(records.len());
                let end = (start + MAX_PEER_RECORD_BATCH).min(records.len());
                PeerResult::Records {
                    records: records[start..end].to_vec(),
                    next: (end < records.len()).then_some(end as u16),
                }
            }
            PeerOperation::Connect {
                manifest_id,
                service,
                port,
            } => {
                let target = own
                    .catalog
                    .services
                    .iter()
                    .find(|item| item.name == service)
                    .and_then(|item| item.ports.iter().find(|mapping| mapping.port == port))
                    .map(|mapping| mapping.target_port);
                if manifest_id == own.catalog.manifest_id
                    && let Some(target) = target
                {
                    match tokio::time::timeout(
                        PEER_TIMEOUT,
                        TcpStream::connect((self.application_host, target)),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => {
                            application = Some(stream);
                            PeerResult::Connected
                        }
                        _ => PeerResult::Rejected,
                    }
                } else {
                    PeerResult::Rejected
                }
            }
        };
        let response = PeerResponse {
            responder: own.clone(),
            result,
        };
        protocol::write_workload_frame(
            &mut send,
            PeerResponse::TYPE.frame_kind(),
            &self.seal(&connection, &response)?,
            PEER_TIMEOUT,
            &self.shutdown,
        )
        .await?;
        if let Some(application) = application {
            let (read, write) = application.into_split();
            iroh_support::raw_relay::supervise_raw_relay(
                recv,
                send,
                read,
                write,
                iroh_support::raw_relay::RawRelayLimits {
                    max_bytes_per_direction: 512 * 1024 * 1024,
                    idle_timeout: Duration::from_secs(300),
                    max_lifetime: Duration::from_secs(3600),
                    authority_interval: Duration::from_secs(30),
                },
                self.shutdown.clone(),
                || {
                    caller.credential.verify(Some((self.clock)()))?;
                    self.own_record()?.credential.verify(Some((self.clock)()))
                },
            )
            .await?;
        } else {
            send.finish()?;
        }
        Ok(())
    }
}
impl ProtocolHandler for PeerState {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let result = self.serve(connection.clone()).await;
        connection.close(0u8.into(), b"peer operation complete");
        result.map_err(|error| {
            log::warn!("peer operation refused: {error:#}");
            AcceptError::from_err(std::io::Error::other("peer operation refused"))
        })
    }
}
