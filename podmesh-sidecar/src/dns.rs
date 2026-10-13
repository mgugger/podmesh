//! Opt-in, loopback-only split DNS. Transport admission is bounded before
//! parsing; Hickory supplies parsing, request/response semantics and encoding.
//! Public forwarding is deliberately non-recursive: an upstream CNAME into the
//! internal suffix must be resolved by this adapter, never followed publicly.
use crate::{peer_connector::Clock, service_cache::ServiceCache};
use anyhow::{Context, Result, ensure};
use hickory_proto::{
    op::{Header, HeaderCounts, Message, MessageType, Metadata, OpCode, ResponseCode},
    rr::{DNSClass, RData, Record, RecordType},
    serialize::binary::BinEncoder,
};
use hickory_server::{
    net::{
        NetError,
        runtime::{Time, TokioTime},
        xfer::Protocol,
    },
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo},
    zone_handler::{MessageResponse, MessageResponseBuilder},
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::Semaphore,
    task::JoinSet,
    time::Instant,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

pub const MAX_DNS_PACKET_BYTES: usize = 4096;
const MAX_UPSTREAMS: usize = 4;
const MAX_FORWARD_CACHE: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct DnsLimits {
    pub max_handlers: usize,
    pub request_timeout: Duration,
}
impl Default for DnsLimits {
    fn default() -> Self {
        Self {
            max_handlers: 32,
            request_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Clone)]
struct CachedAnswer {
    message: Message,
    expires: Instant,
}

pub struct DnsForwarder {
    servers: Vec<SocketAddr>,
    bypass_mark: Option<u32>,
    cache: Mutex<HashMap<String, CachedAnswer>>,
}
impl DnsForwarder {
    pub fn new(servers: Vec<SocketAddr>) -> Result<Self> {
        ensure!(
            !servers.is_empty()
                && servers.len() <= MAX_UPSTREAMS
                && servers
                    .iter()
                    .all(|server| server.port() != 0 && !server.ip().is_unspecified()),
            "invalid upstream DNS configuration"
        );
        Ok(Self {
            servers,
            bypass_mark: None,
            cache: Mutex::new(HashMap::new()),
        })
    }
    pub fn with_bypass_mark(mut self, mark: u32) -> Self {
        self.bypass_mark = Some(mark);
        self
    }
    /// Capture the workload's original resolver configuration, before any future
    /// interception is enabled. No hardcoded public resolver or search expansion.
    pub fn from_system() -> Result<Self> {
        let (config, _) = hickory_resolver::system_conf::read_system_conf()?;
        let servers = config
            .name_servers()
            .iter()
            .map(|server| {
                SocketAddr::new(
                    server.ip,
                    server
                        .connections
                        .first()
                        .map(|connection| connection.port)
                        .unwrap_or(53),
                )
            })
            .take(MAX_UPSTREAMS)
            .collect();
        Self::new(servers)
    }
    fn cache_key(query: &hickory_proto::op::Query) -> String {
        format!(
            "{}:{:?}:{:?}",
            query.name().to_lowercase(),
            query.query_type(),
            query.query_class()
        )
    }
    async fn lookup(&self, query: hickory_proto::op::Query) -> Result<Message> {
        let key = Self::cache_key(&query);
        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("DNS cache poisoned"))?;
            cache.retain(|_, answer| answer.expires > Instant::now());
            if let Some(answer) = cache.get(&key) {
                let mut message = answer.message.clone();
                let ttl = answer
                    .expires
                    .saturating_duration_since(Instant::now())
                    .as_secs() as u32;
                for record in message
                    .answers
                    .iter_mut()
                    .chain(message.authorities.iter_mut())
                    .chain(message.additionals.iter_mut())
                {
                    record.ttl = record.ttl.min(ttl);
                }
                return Ok(message);
            }
        }
        let nonce = crypto::generate_secure_nonce();
        let digest = sha2::Sha256::digest(nonce.as_bytes());
        let id = u16::from_be_bytes([digest[0], digest[1]]);
        let mut request = Message::new(id, MessageType::Query, OpCode::Query);
        request.metadata.recursion_desired = true;
        request.queries.push(query.clone());
        let bytes = request.to_vec()?;
        for server in &self.servers {
            let result = async {
                let bind = if server.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                let socket = UdpSocket::bind(bind).await?;
                if let Some(mark) = self.bypass_mark {
                    mark_socket(&socket, mark)?;
                }
                socket.connect(server).await?;
                socket.send(&bytes).await?;
                let mut buffer = vec![0; MAX_DNS_PACKET_BYTES + 1];
                let len = socket.recv(&mut buffer).await?;
                ensure!(
                    len <= MAX_DNS_PACKET_BYTES,
                    "upstream DNS response exceeds limit"
                );
                let mut response = Message::from_vec(&buffer[..len])?;
                check_upstream(&response, id, &query)?;
                if response.metadata.truncation {
                    let socket = if server.is_ipv4() {
                        tokio::net::TcpSocket::new_v4()?
                    } else {
                        tokio::net::TcpSocket::new_v6()?
                    };
                    if let Some(mark) = self.bypass_mark {
                        mark_socket(&socket, mark)?;
                    }
                    let mut socket = socket.connect(*server).await?;
                    socket.write_u16(bytes.len() as u16).await?;
                    socket.write_all(&bytes).await?;
                    let size = socket.read_u16().await? as usize;
                    ensure!(
                        (12..=MAX_DNS_PACKET_BYTES).contains(&size),
                        "upstream TCP DNS response exceeds limit"
                    );
                    buffer.resize(size, 0);
                    socket.read_exact(&mut buffer).await?;
                    response = Message::from_vec(&buffer)?;
                    check_upstream(&response, id, &query)?;
                    ensure!(
                        !response.metadata.truncation,
                        "upstream TCP DNS response is truncated"
                    );
                }
                Ok::<_, anyhow::Error>(response)
            };
            if let Ok(Ok(response)) = tokio::time::timeout(Duration::from_millis(500), result).await
            {
                let ttl = response
                    .answers
                    .iter()
                    .chain(response.authorities.iter())
                    .map(|record| record.ttl)
                    .min()
                    .unwrap_or(0)
                    .min(5);
                if ttl > 0
                    && matches!(
                        response.metadata.response_code,
                        ResponseCode::NoError | ResponseCode::NXDomain
                    )
                {
                    let mut cache = self
                        .cache
                        .lock()
                        .map_err(|_| anyhow::anyhow!("DNS cache poisoned"))?;
                    if cache.len() >= MAX_FORWARD_CACHE {
                        cache.clear();
                    }
                    cache.insert(
                        key,
                        CachedAnswer {
                            message: response.clone(),
                            expires: Instant::now() + Duration::from_secs(u64::from(ttl)),
                        },
                    );
                }
                return Ok(response);
            }
        }
        anyhow::bail!("no upstream DNS answer available")
    }
}
use sha2::Digest;
fn mark_socket(socket: &impl std::os::fd::AsRawFd, mark: u32) -> Result<()> {
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_MARK,
            (&mark as *const u32).cast(),
            std::mem::size_of::<u32>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("set DNS upstream bypass mark");
    }
    Ok(())
}
fn check_upstream(response: &Message, id: u16, query: &hickory_proto::op::Query) -> Result<()> {
    ensure!(
        response.metadata.id == id
            && response.metadata.message_type == MessageType::Response
            && response.metadata.op_code == OpCode::Query
            && response.queries.as_slice() == [query.clone()],
        "upstream DNS response does not match query"
    );
    Ok(())
}

#[derive(Clone)]
struct BufferResponse {
    bytes: Arc<Mutex<Option<Vec<u8>>>>,
    maximum: u16,
}
#[async_trait::async_trait]
impl ResponseHandler for BufferResponse {
    async fn send_response<'a>(
        &mut self,
        response: MessageResponse<
            '_,
            'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
        >,
    ) -> Result<ResponseInfo, NetError> {
        let mut bytes = Vec::new();
        let mut encoder = BinEncoder::new(&mut bytes);
        encoder.set_max_size(self.maximum);
        let info = response.destructive_emit(&mut encoder)?;
        *self
            .bytes
            .lock()
            .map_err(|_| NetError::Message("DNS response buffer poisoned"))? = Some(bytes);
        Ok(info)
    }
}

pub struct DnsHandler {
    cache: Arc<Mutex<ServiceCache>>,
    upstream: Arc<DnsForwarder>,
    clock: Clock,
    timeout: Duration,
}
#[async_trait::async_trait]
impl RequestHandler for DnsHandler {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &Request,
        mut response: R,
    ) -> ResponseInfo {
        let mut metadata = Metadata::response_from_request(&request.metadata);
        metadata.authentic_data = false;
        metadata.recursion_available = true;
        let mut answers = Vec::new();
        let mut authorities = Vec::new();
        let mut additionals = Vec::new();
        let result: Result<()> = async {
            let info = match request.request_info() {
                Ok(info) => info,
                Err(_) => {
                    metadata.response_code = ResponseCode::FormErr;
                    return Ok(());
                }
            };
            if request.as_slice().len() > MAX_DNS_PACKET_BYTES
                || request.metadata.message_type != MessageType::Query
                || request.metadata.op_code != OpCode::Query
                || !request.answers.is_empty()
                || info.query.query_class() != DNSClass::IN
                || matches!(
                    info.query.query_type(),
                    RecordType::AXFR | RecordType::IXFR | RecordType::ANY
                )
            {
                metadata.response_code = ResponseCode::Refused;
                return Ok(());
            }
            let name = info.query.name().to_string();
            let internal = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("service cache poisoned"))?
                .classifies_internal(&name);
            if internal {
                let cache = self
                    .cache
                    .lock()
                    .map_err(|_| anyhow::anyhow!("service cache poisoned"))?;
                if let Some(answer) = cache.lookup(&name, (self.clock)()) {
                    metadata.authoritative = true;
                    if info.query.query_type() == RecordType::A {
                        answers.push(Record::from_rdata(
                            info.query.original().name().clone(),
                            answer.ttl,
                            RData::A(answer.address.into()),
                        ));
                    }
                    // IPv4-only service: AAAA/other record types have NODATA,
                    // not a fabricated address or a fallback to public DNS.
                } else {
                    metadata.response_code = ResponseCode::ServFail;
                }
            } else {
                if !request.metadata.recursion_desired {
                    metadata.response_code = ResponseCode::Refused;
                    return Ok(());
                }
                let message = tokio::time::timeout(
                    self.timeout,
                    self.upstream.lookup(info.query.original().clone()),
                )
                .await??;
                metadata.response_code = message.metadata.response_code;
                answers = message.answers;
                authorities = message.authorities;
                additionals = message.additionals;
                // Do not chase CNAMEs. The client can ask this split resolver
                // for the alias target, preserving the internal trust boundary.
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            metadata.response_code = ResponseCode::ServFail;
        }
        let mut edns = request.edns.clone();
        if let Some(edns) = &mut edns {
            edns.set_max_payload(edns.max_payload().min(1232));
            edns.set_dnssec_ok(false);
        }
        let message = MessageResponseBuilder::new(&request.queries, edns.as_ref()).build(
            metadata,
            answers.iter(),
            authorities.iter(),
            std::iter::empty(),
            additionals.iter(),
        );
        match response.send_response(message).await {
            Ok(info) => info,
            Err(_) => {
                metadata.response_code = ResponseCode::ServFail;
                ResponseInfo::from(Header {
                    metadata,
                    counts: HeaderCounts::default(),
                })
            }
        }
    }
}

pub struct LocalDns {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: AbortOnDropHandle<Result<()>>,
}
impl LocalDns {
    pub async fn bind(
        address: SocketAddr,
        cache: Arc<Mutex<ServiceCache>>,
        upstream: Arc<DnsForwarder>,
        clock: Clock,
        limits: DnsLimits,
    ) -> Result<Self> {
        ensure!(
            address.ip().is_loopback(),
            "local DNS must bind loopback only"
        );
        ensure!(
            (1..=128).contains(&limits.max_handlers)
                && !limits.request_timeout.is_zero()
                && limits.request_timeout <= Duration::from_secs(10),
            "invalid DNS limits"
        );
        let tcp = TcpListener::bind(address).await?;
        let address = tcp.local_addr()?;
        let udp = Arc::new(UdpSocket::bind(address).await?);
        ensure!(
            !upstream.servers.contains(&address),
            "DNS upstream forwarding loop"
        );
        let handler = Arc::new(DnsHandler {
            cache,
            upstream,
            clock,
            timeout: limits.request_timeout,
        });
        let shutdown = CancellationToken::new();
        let cancellation = shutdown.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(limits.max_handlers));
            let mut handlers = JoinSet::new();
            let mut bytes = vec![0; MAX_DNS_PACKET_BYTES + 1];
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    Some(_) = handlers.join_next(), if !handlers.is_empty() => {},
                    result = udp.recv_from(&mut bytes) => {
                        let (size, source) = result?;
                        if size > MAX_DNS_PACKET_BYTES { continue; }
                        let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                        let bytes = bytes[..size].to_vec(); let socket = udp.clone(); let handler = handler.clone();
                        handlers.spawn(async move {
                            let _permit = permit;
                            let _ = tokio::time::timeout(limits.request_timeout, async {
                                let response = handle_packet(&handler, bytes, source, Protocol::Udp).await?;
                                socket.send_to(&response, source).await?;
                                Ok::<_, anyhow::Error>(())
                            }).await;
                        });
                    }
                    result = tcp.accept() => {
                        let (mut stream, source) = result?;
                        let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                        let handler = handler.clone();
                        handlers.spawn(async move {
                            let _permit = permit;
                            let _ = tokio::time::timeout(limits.request_timeout, async {
                                let size = stream.read_u16().await? as usize;
                                ensure!((12..=MAX_DNS_PACKET_BYTES).contains(&size), "invalid DNS TCP frame size");
                                let mut bytes = vec![0; size]; stream.read_exact(&mut bytes).await?;
                                let response = handle_packet(&handler, bytes, source, Protocol::Tcp).await?;
                                stream.write_u16(response.len() as u16).await?; stream.write_all(&response).await?;
                                stream.shutdown().await?;
                                Ok::<_, anyhow::Error>(())
                            }).await;
                        });
                    }
                }
            }
            handlers.abort_all();
            while handlers.join_next().await.is_some() {}
            Ok(())
        }));
        Ok(Self {
            address,
            shutdown,
            task,
        })
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub async fn shutdown(self) -> Result<()> {
        self.shutdown.cancel();
        self.task.await??;
        Ok(())
    }
}
async fn handle_packet(
    handler: &DnsHandler,
    bytes: Vec<u8>,
    source: SocketAddr,
    protocol: Protocol,
) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() <= MAX_DNS_PACKET_BYTES,
        "DNS packet exceeds limit"
    );
    let request = Request::from_bytes(bytes, source, protocol)?;
    let maximum = if protocol == Protocol::Udp {
        request
            .edns
            .as_ref()
            .map(|edns| edns.max_payload())
            .unwrap_or(512)
            .min(1232)
    } else {
        MAX_DNS_PACKET_BYTES as u16
    };
    let response = BufferResponse {
        bytes: Arc::new(Mutex::new(None)),
        maximum,
    };
    handler
        .handle_request::<_, TokioTime>(&request, response.clone())
        .await;
    response
        .bytes
        .lock()
        .map_err(|_| anyhow::anyhow!("DNS response buffer poisoned"))?
        .take()
        .context("DNS response was not produced")
}
