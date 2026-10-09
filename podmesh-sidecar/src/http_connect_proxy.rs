//! HTTP CONNECT proxy for explicit proxy mode
//!
//! This module provides an HTTP proxy that handles CONNECT requests,
//! tunneling traffic through the egress tunnel to the proxy node.
//! This allows applications to use standard HTTP_PROXY environment
//! variable or explicit proxy configuration.

use anyhow::{Context, Result};
use protocol::egress::EgressProtocol;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};

use crate::egress_proxy::TunnelRequest;

/// Default port for the HTTP CONNECT proxy
pub const HTTP_CONNECT_PROXY_PORT: u16 = 15080;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_PENDING_HANDLERS: usize = 64;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(1);
const ERROR_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct AdmissionLimits {
    head_bytes: usize,
    head_timeout: Duration,
    handlers: usize,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            head_bytes: MAX_HEAD_BYTES,
            head_timeout: HEAD_TIMEOUT,
            handlers: MAX_PENDING_HANDLERS,
        }
    }
}

#[derive(Debug)]
enum HeadError {
    Invalid,
    TooLarge,
}

impl std::fmt::Display for HeadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "invalid HTTP head",
            Self::TooLarge => "HTTP head exceeds limit",
        })
    }
}
impl std::error::Error for HeadError {}

struct RequestHead {
    request_line: String,
    headers: Vec<String>,
}

fn parse_head(bytes: &[u8]) -> Result<RequestHead> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    let result = request.parse(bytes).map_err(|error| match error {
        httparse::Error::TooManyHeaders => HeadError::TooLarge,
        _ => HeadError::Invalid,
    })?;
    anyhow::ensure!(result.is_complete(), HeadError::Invalid);
    let request_line = format!(
        "{} {} HTTP/1.{}",
        request.method.ok_or(HeadError::Invalid)?,
        request.path.ok_or(HeadError::Invalid)?,
        request.version.ok_or(HeadError::Invalid)?
    );
    let headers = request
        .headers
        .iter()
        .map(|header| {
            let value = std::str::from_utf8(header.value).map_err(|_| HeadError::Invalid)?;
            Ok(format!("{}: {}\r\n", header.name, value))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RequestHead {
        request_line,
        headers,
    })
}

async fn read_head<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    maximum: usize,
) -> Result<RequestHead> {
    let mut bytes = Vec::with_capacity(maximum.min(MAX_HEAD_BYTES));
    loop {
        anyhow::ensure!(bytes.len() < maximum, HeadError::TooLarge);
        let available = reader.fill_buf().await?;
        anyhow::ensure!(!available.is_empty(), HeadError::Invalid);
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1)
            .min(maximum - bytes.len());
        bytes.extend_from_slice(&available[..length]);
        reader.consume(length);
        if bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n") {
            return parse_head(&bytes);
        }
    }
}

/// Configuration for the HTTP CONNECT proxy
#[derive(Debug, Clone)]
pub struct HttpConnectProxyConfig {
    /// Port to listen on for HTTP CONNECT requests
    pub listen_port: u16,
    /// Host to bind to
    pub listen_host: String,
}

impl Default for HttpConnectProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: HTTP_CONNECT_PROXY_PORT,
            listen_host: "127.0.0.1".to_string(),
        }
    }
}

/// HTTP CONNECT proxy that forwards connections through the egress tunnel
pub struct HttpConnectProxy {
    config: HttpConnectProxyConfig,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
}

impl HttpConnectProxy {
    /// Creates a new HTTP CONNECT proxy
    pub fn new(config: HttpConnectProxyConfig, tunnel_tx: mpsc::Sender<TunnelRequest>) -> Self {
        Self { config, tunnel_tx }
    }

    /// Starts the HTTP CONNECT proxy listener
    pub async fn run(&self) -> Result<()> {
        let addr: SocketAddr = format!("{}:{}", self.config.listen_host, self.config.listen_port)
            .parse()
            .context("invalid listen address")?;

        let listener = TcpListener::bind(addr)
            .await
            .context("failed to bind HTTP CONNECT proxy")?;

        log::info!(
            "HTTP CONNECT proxy listening on {}:{}",
            self.config.listen_host,
            self.config.listen_port
        );

        let limits = AdmissionLimits::default();
        self.serve(listener, limits, Arc::new(Semaphore::new(limits.handlers)))
            .await
    }

    async fn serve(
        &self,
        listener: TcpListener,
        limits: AdmissionLimits,
        slots: Arc<Semaphore>,
    ) -> Result<()> {
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
                incoming = listener.accept() => match incoming {
                Ok((stream, peer_addr)) => {
                    let accepted_at = Instant::now();
                    while handlers.try_join_next().is_some() {}
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                    let tunnel_tx = self.tunnel_tx.clone();
                    handlers.spawn(async move {
                        let _permit = permit;
                        if handle_connection_at(stream, tunnel_tx, accepted_at + limits.head_timeout, limits.head_bytes).await.is_err() {
                            log::debug!("HTTP proxy connection refused peer={peer_addr}");
                        }
                    });
                }
                Err(err) => {
                    log::warn!("failed to accept HTTP CONNECT connection: {}", err);
                }
                }
            }
        }
    }

    /// Returns the listen address
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.config.listen_host, self.config.listen_port)
    }
}

/// Handle an incoming HTTP CONNECT proxy connection
#[cfg(test)]
async fn handle_connection(
    stream: TcpStream,
    _peer_addr: SocketAddr,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
) -> Result<()> {
    handle_connection_at(
        stream,
        tunnel_tx,
        Instant::now() + HEAD_TIMEOUT,
        MAX_HEAD_BYTES,
    )
    .await
}

async fn handle_connection_at(
    stream: TcpStream,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
    deadline: Instant,
    head_bytes: usize,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let head = match timeout_at(deadline, read_head(&mut buf_reader, head_bytes)).await {
        Err(_) => return Ok(()),
        Ok(Err(error)) => {
            if matches!(error.downcast_ref::<HeadError>(), Some(HeadError::TooLarge)) {
                send_error(&mut writer, 431, "Request Header Fields Too Large").await?;
            } else {
                send_error(&mut writer, 400, "Bad Request").await?;
            }
            return Ok(());
        }
        Ok(Ok(head)) => head,
    };
    let request_line = head.request_line;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() != 3 {
        send_error(&mut writer, 400, "Bad Request").await?;
        return Ok(());
    }

    let method = parts[0];
    let target = parts[1];
    let valid_target = if method == "CONNECT" {
        parse_host_port(target).map(|(host, port)| !host.is_empty() && port != 0)
    } else {
        parse_proxy_url(target).map(|(_, port, _)| port != 0)
    };
    if !matches!(valid_target, Ok(true)) {
        send_error(&mut writer, 400, "Bad Request").await?;
        return Ok(());
    }

    if method == "CONNECT" {
        // Handle CONNECT method (tunneling for HTTPS)
        handle_connect(target, buf_reader, writer, tunnel_tx).await
    } else {
        // Handle plain HTTP proxy (GET, POST, etc.)
        handle_http_proxy(&request_line, &head.headers, buf_reader, writer, tunnel_tx).await
    }
}

/// Handle HTTP CONNECT request
async fn handle_connect(
    target: &str,
    buf_reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
) -> Result<()> {
    // Parse host:port from target
    let (host, port) = parse_host_port(target)?;

    // Reunite the stream for the tunnel
    let initial_data = buf_reader.buffer().to_vec();
    let reader = buf_reader.into_inner();
    let stream = reader.reunite(writer).context("failed to reunite stream")?;

    // Create tunnel request
    let tunnel_req = TunnelRequest {
        dest_host: host,
        dest_port: port,
        protocol: EgressProtocol::Tcp,
        client_stream: stream,
        send_http_200: true, // HTTP CONNECT needs 200 response
        initial_data: (!initial_data.is_empty()).then_some(initial_data),
    };

    // Send to tunnel handler
    handoff(tunnel_req, tunnel_tx).await
}

/// Handle plain HTTP proxy request (GET, POST, etc.)
///
/// For plain HTTP, we open a tunnel to the target host:port and forward
/// the original request through it.
async fn handle_http_proxy(
    request_line: &str,
    headers: &[String],
    buf_reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
) -> Result<()> {
    // Parse the URL from the request line (e.g., "GET http://example.com:8080/path HTTP/1.1")
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(anyhow::anyhow!("invalid request line"));
    }

    let method = parts[0];
    let url = parts[1];
    let version = parts[2];

    // Parse the URL to extract host, port, and path
    let (host, port, path) = parse_proxy_url(url)?;

    // Reunite the stream
    let buffered_body = buf_reader.buffer().to_vec();
    let reader = buf_reader.into_inner();
    let stream = reader.reunite(writer).context("failed to reunite stream")?;

    // Build the modified request to send through tunnel
    // Convert absolute URL to relative path for the origin server
    let modified_request_line = format!("{} {} {}\r\n", method, path, version);

    // Prepare the full request to write after tunnel is established
    let mut request_bytes = modified_request_line.into_bytes();
    for header in headers {
        // Skip Proxy-* headers
        if !header.to_lowercase().starts_with("proxy-") {
            request_bytes.extend_from_slice(header.as_bytes());
        }
    }
    request_bytes.extend_from_slice(b"\r\n");
    request_bytes.extend_from_slice(&buffered_body);

    // Create tunnel request with initial data to send to destination
    let tunnel_req = TunnelRequest {
        dest_host: host,
        dest_port: port,
        protocol: EgressProtocol::Tcp,
        client_stream: stream,
        send_http_200: false, // Plain HTTP proxy doesn't need 200 response
        initial_data: Some(request_bytes), // Forward the HTTP request through tunnel
    };

    // Send to tunnel handler
    handoff(tunnel_req, tunnel_tx).await
}

async fn handoff(mut request: TunnelRequest, sender: mpsc::Sender<TunnelRequest>) -> Result<()> {
    match timeout(HANDOFF_TIMEOUT, sender.reserve()).await {
        Ok(Ok(permit)) => {
            permit.send(request);
            Ok(())
        }
        _ => send_error(&mut request.client_stream, 503, "Service Unavailable").await,
    }
}

/// Parse a proxy URL like "http://host:port/path" into (host, port, path)
fn parse_proxy_url(url: &str) -> Result<(String, u16, String)> {
    let parsed = reqwest::Url::parse(url).context("invalid HTTP proxy URL")?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "unsupported HTTP proxy URL scheme"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "HTTP proxy URL must not contain credentials"
    );
    let host = parsed
        .host_str()
        .context("HTTP proxy URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = parsed
        .port_or_known_default()
        .context("HTTP proxy URL has no port")?;
    let mut path = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    Ok((host, port, path))
}

/// Parse host:port from a target string
fn parse_host_port(target: &str) -> Result<(String, u16)> {
    // Handle IPv6 addresses in brackets: [::1]:8080
    if target.starts_with('[') {
        if let Some(bracket_end) = target.find(']') {
            let host = &target[1..bracket_end];
            let port_part = &target[bracket_end + 1..];
            if let Some(port_part) = port_part.strip_prefix(':') {
                let port: u16 = port_part
                    .parse()
                    .context("invalid port in CONNECT target")?;
                return Ok((host.to_string(), port));
            }
        }
        anyhow::bail!("invalid IPv6 CONNECT target");
    }

    // Handle regular host:port
    if let Some(colon_pos) = target.rfind(':') {
        let host = &target[..colon_pos];
        let port: u16 = target[colon_pos + 1..]
            .parse()
            .context("invalid port in CONNECT target")?;
        Ok((host.to_string(), port))
    } else {
        // Default to port 443 for HTTPS
        Ok((target.to_string(), 443))
    }
}

/// Send an HTTP error response
async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    message: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status, message
    );
    timeout(ERROR_WRITE_TIMEOUT, writer.write_all(response.as_bytes()))
        .await
        .context("HTTP error response timed out")?
        .context("failed to write error response")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn request_head_byte_and_header_limits_are_exact() -> Result<()> {
        let head = b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n";
        let mut reader = BufReader::new(head.as_slice());
        assert!(read_head(&mut reader, head.len()).await.is_ok());
        let mut reader = BufReader::new(head.as_slice());
        assert!(matches!(
            read_head(&mut reader, head.len() - 1)
                .await
                .err()
                .unwrap()
                .downcast_ref::<HeadError>(),
            Some(HeadError::TooLarge)
        ));
        for count in [MAX_HEADERS, MAX_HEADERS + 1] {
            let head = format!("GET / HTTP/1.1\r\n{}\r\n", "X: v\r\n".repeat(count));
            assert_eq!(parse_head(head.as_bytes()).is_ok(), count == MAX_HEADERS);
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn whole_head_deadline_is_not_reset_by_partial_progress() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut client = TcpStream::connect(listener.local_addr()?).await?;
        let (server, _) = listener.accept().await?;
        let (sender, mut receiver) = mpsc::channel(1);
        let task = tokio::spawn(handle_connection_at(
            server,
            sender,
            Instant::now() + HEAD_TIMEOUT,
            MAX_HEAD_BYTES,
        ));
        client
            .write_all(b"GET http://example.test/ HTTP/1.1\r\n")
            .await?;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(4)).await;
        client.write_all(b"X:").await?;
        tokio::time::advance(Duration::from_secs(2)).await;
        task.await??;
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn full_and_closed_queues_fail_with_bounded_503() -> Result<()> {
        for closed in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let mut client = TcpStream::connect(listener.local_addr()?).await?;
            let (server, _) = listener.accept().await?;
            let (sender, receiver) = mpsc::channel(1);
            let slot = sender.clone().reserve_owned().await?;
            if closed {
                drop(receiver);
            }
            let task = tokio::spawn(handle_connection_at(
                server,
                sender,
                Instant::now() + HEAD_TIMEOUT,
                MAX_HEAD_BYTES,
            ));
            client
                .write_all(b"CONNECT example.test:443 HTTP/1.1\r\n\r\n")
                .await?;
            let mut response = String::new();
            timeout(Duration::from_secs(3), client.read_to_string(&mut response)).await??;
            task.await??;
            assert!(response.starts_with("HTTP/1.1 503"));
            assert!(!response.contains("200"));
            drop(slot);
        }
        Ok(())
    }

    #[tokio::test]
    async fn listener_saturation_shutdown_and_permit_recovery_are_bounded() -> Result<()> {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let slots = Arc::new(Semaphore::new(1));
            let observed = slots.clone();
            let (sender, mut receiver) = mpsc::channel(1);
            let proxy = HttpConnectProxy::new(HttpConnectProxyConfig::default(), sender);
            let task = tokio::spawn(async move {
                proxy
                    .serve(listener, AdmissionLimits::default(), slots)
                    .await
            });
            let mut first = TcpStream::connect(address).await?;
            while observed.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
            let mut excess = TcpStream::connect(address).await?;
            let mut byte = [0];
            assert_eq!(excess.read(&mut byte).await?, 0);
            first
                .write_all(b"CONNECT example.test:443 HTTP/1.1\r\n\r\n")
                .await?;
            let tunnel = receiver.recv().await.context("missing accepted tunnel")?;
            assert!(tunnel.send_http_200);
            while observed.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
            let mut partial = TcpStream::connect(address).await?;
            while observed.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(partial.read(&mut byte).await?, 0);
            assert_eq!(observed.available_permits(), 1);
            Ok(())
        })
        .await?
    }

    #[tokio::test(start_paused = true)]
    async fn error_writes_cannot_hold_a_handler_forever() {
        let (mut writer, _unread) = tokio::io::duplex(1);
        let started = Instant::now();
        assert!(send_error(&mut writer, 400, "Bad Request").await.is_err());
        assert!(started.elapsed() <= ERROR_WRITE_TIMEOUT);
    }

    #[tokio::test]
    async fn listener_shutdown_cancels_pending_queue_handoff() -> Result<()> {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let (sender, _receiver) = mpsc::channel(1);
            let queue_slot = sender.clone().reserve_owned().await?;
            let slots = Arc::new(Semaphore::new(1));
            let observed = slots.clone();
            let proxy = HttpConnectProxy::new(HttpConnectProxyConfig::default(), sender);
            let task = tokio::spawn(async move {
                proxy
                    .serve(listener, AdmissionLimits::default(), slots)
                    .await
            });
            let mut client = TcpStream::connect(address).await?;
            client
                .write_all(b"CONNECT example.test:443 HTTP/1.1\r\n\r\n")
                .await?;
            while observed.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let mut response = String::new();
            let _ = client.read_to_string(&mut response).await;
            while observed.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
            assert!(!response.contains("200"));
            drop(queue_slot);
            Ok(())
        })
        .await?
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 256,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x504f_444d_4553_4807),
            ..proptest::test_runner::Config::default()
        })]
        #[test]
        fn fragmented_heads_preserve_prefetched_body(
            header in "[a-z0-9]{0,64}",
            body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..128),
            fragment in 1usize..32,
        ) {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let head = format!("POST http://example.test/upload HTTP/1.1\r\nX-Value: {header}\r\nContent-Length: {}\r\n\r\n", body.len());
                let mut request = head.as_bytes().to_vec();
                request.extend_from_slice(&body);
                let (mut writer, reader) = tokio::io::duplex(fragment);
                let write = async { for chunk in request.chunks(fragment) { writer.write_all(chunk).await.unwrap(); } writer.shutdown().await.unwrap(); };
                let read = async {
                    let mut buffered = BufReader::new(reader);
                    let parsed = read_head(&mut buffered, head.len()).await.unwrap();
                    assert!(parsed.request_line.starts_with("POST "));
                    let mut observed = Vec::new();
                    buffered.read_to_end(&mut observed).await.unwrap();
                    assert_eq!(observed, body);
                };
                tokio::join!(write, read);
            });
        }
    }

    #[tokio::test]
    async fn invalid_heads_never_reach_the_tunnel_queue() -> Result<()> {
        for request in [
            b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test\r\n".to_vec(),
            format!(
                "CONNECT example.test:443 HTTP/1.1\r\nX-Large: {}\r\n\r\n",
                "a".repeat(16 * 1024)
            )
            .into_bytes(),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let mut client = TcpStream::connect(listener.local_addr()?).await?;
            let (server, peer) = listener.accept().await?;
            let (sender, mut receiver) = mpsc::channel(1);
            let task = tokio::spawn(handle_connection(server, peer, sender));
            client.write_all(&request).await?;
            client.shutdown().await?;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await??;
            assert!(
                receiver.try_recv().is_err(),
                "invalid HTTP head reached the tunnel queue"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_or_oversized_heads_never_reach_the_tunnel_queue() -> Result<()> {
        for request in [
            "GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n".to_string(),
            format!(
                "GET http://example.test/ HTTP/1.1\r\nX-Large: {}\r\n\r\n",
                "a".repeat(16 * 1024)
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let mut client = TcpStream::connect(listener.local_addr()?).await?;
            let (server, peer) = listener.accept().await?;
            let (sender, mut receiver) = mpsc::channel(1);
            let task = tokio::spawn(handle_connection(server, peer, sender));
            client.write_all(request.as_bytes()).await?;
            client.shutdown().await?;
            tokio::time::timeout(std::time::Duration::from_secs(3), task).await???;
            assert!(
                receiver.try_recv().is_err(),
                "invalid head must not create a tunnel"
            );
        }
        Ok(())
    }

    #[test]
    fn proxy_urls_use_scheme_ports_and_preserve_request_targets() {
        for (input, host, port, path) in [
            ("http://example.test", "example.test", 80, "/"),
            ("https://example.test", "example.test", 443, "/"),
            (
                "http://example.test?key=value",
                "example.test",
                80,
                "/?key=value",
            ),
            (
                "http://[::1]:8080/a%20b?q=1#fragment",
                "::1",
                8080,
                "/a%20b?q=1",
            ),
        ] {
            assert_eq!(
                parse_proxy_url(input).unwrap(),
                (host.into(), port, path.into())
            );
        }
        for invalid in [
            "",
            "/relative",
            "ftp://example.test",
            "http://user:password@example.test",
            "http://example.test:invalid",
        ] {
            assert!(parse_proxy_url(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn tunnel_handoff_preserves_prefetched_client_bytes() -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for connect in [true, false] {
                let listener = TcpListener::bind("127.0.0.1:0").await?;
                let mut client = TcpStream::connect(listener.local_addr()?).await?;
                let (server, _) = listener.accept().await?;
                client.write_all(b"body").await?;
                client.shutdown().await?;
                let (reader, writer) = server.into_split();
                let mut buffered = BufReader::new(reader);
                buffered.fill_buf().await?;
                anyhow::ensure!(!buffered.buffer().is_empty());
                let (sender, mut receiver) = mpsc::channel(1);
                if connect {
                    handle_connect("example.test:443", buffered, writer, sender).await?;
                } else {
                    handle_http_proxy(
                        "POST http://example.test:80/upload HTTP/1.1",
                        &["Content-Length: 4\r\n".into()],
                        buffered,
                        writer,
                        sender,
                    )
                    .await?;
                }
                let mut tunnel = receiver.recv().await.context("missing tunnel handoff")?;
                let mut forwarded = tunnel.initial_data.take().unwrap_or_default();
                tokio::io::AsyncReadExt::read_to_end(&mut tunnel.client_stream, &mut forwarded)
                    .await?;
                let expected = if connect {
                    b"body".to_vec()
                } else {
                    b"POST /upload HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody".to_vec()
                };
                assert_eq!(forwarded, expected);
                assert_eq!(tunnel.send_http_200, connect);
            }
            Ok(())
        })
        .await
        .context("tunnel handoff regression timed out")?
    }

    #[test]
    fn test_parse_host_port() {
        assert_eq!(
            parse_host_port("example.com:443").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            parse_host_port("example.com:8080").unwrap(),
            ("example.com".to_string(), 8080)
        );
        assert_eq!(
            parse_host_port("example.com").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            parse_host_port("[::1]:8080").unwrap(),
            ("::1".to_string(), 8080)
        );
    }
}
