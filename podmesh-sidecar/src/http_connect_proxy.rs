//! HTTP CONNECT proxy for explicit proxy mode
//!
//! This module provides an HTTP proxy that handles CONNECT requests,
//! tunneling traffic through the egress tunnel to the proxy node.
//! This allows applications to use standard HTTP_PROXY environment
//! variable or explicit proxy configuration.

use anyhow::{Context, Result};
use protocol::egress::EgressProtocol;
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::egress_proxy::TunnelRequest;

/// Default port for the HTTP CONNECT proxy
pub const HTTP_CONNECT_PROXY_PORT: u16 = 15080;

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

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let tunnel_tx = self.tunnel_tx.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(stream, peer_addr, tunnel_tx).await {
                            log::debug!(
                                "HTTP CONNECT connection error peer={}: {}",
                                peer_addr,
                                err
                            );
                        }
                    });
                }
                Err(err) => {
                    log::warn!("failed to accept HTTP CONNECT connection: {}", err);
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
async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    // Read the HTTP request line
    let mut request_line = String::new();
    buf_reader
        .read_line(&mut request_line)
        .await
        .context("failed to read request line")?;

    let request_line = request_line.trim().to_string();
    log::debug!("HTTP proxy request from {}: {}", peer_addr, request_line);

    // Parse the request
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        send_error(&mut writer, 400, "Bad Request").await?;
        return Ok(());
    }

    let method = parts[0];
    let target = parts[1];

    // Collect headers
    let mut headers = Vec::new();
    loop {
        let mut header_line = String::new();
        buf_reader
            .read_line(&mut header_line)
            .await
            .context("failed to read header")?;
        if header_line.trim().is_empty() {
            break;
        }
        headers.push(header_line);
    }

    if method == "CONNECT" {
        // Handle CONNECT method (tunneling for HTTPS)
        handle_connect(target, buf_reader, writer, tunnel_tx).await
    } else {
        // Handle plain HTTP proxy (GET, POST, etc.)
        handle_http_proxy(&request_line, &headers, buf_reader, writer, tunnel_tx).await
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

    log::info!("HTTP CONNECT tunnel request to {}:{}", host, port);

    // Reunite the stream for the tunnel
    let reader = buf_reader.into_inner();
    let stream = reader.reunite(writer).context("failed to reunite stream")?;

    // Create tunnel request
    let tunnel_req = TunnelRequest {
        dest_host: host,
        dest_port: port,
        protocol: EgressProtocol::Tcp,
        client_stream: stream,
        send_http_200: true, // HTTP CONNECT needs 200 response
        initial_data: None,
    };

    // Send to tunnel handler
    tunnel_tx
        .send(tunnel_req)
        .await
        .map_err(|_| anyhow::anyhow!("tunnel channel closed"))?;

    Ok(())
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

    log::info!("HTTP proxy request {} {}:{}{}", method, host, port, path);

    // Reunite the stream
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
    tunnel_tx
        .send(tunnel_req)
        .await
        .map_err(|_| anyhow::anyhow!("tunnel channel closed"))?;

    Ok(())
}

/// Parse a proxy URL like "http://host:port/path" into (host, port, path)
fn parse_proxy_url(url: &str) -> Result<(String, u16, String)> {
    // Remove http:// or https:// prefix
    let url = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // Split into host:port and path
    let (host_port, path) = if let Some(slash_pos) = url.find('/') {
        (&url[..slash_pos], &url[slash_pos..])
    } else {
        (url, "/")
    };

    // Parse host and port
    let (host, port) = parse_host_port(host_port)?;

    Ok((host, port, path.to_string()))
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
        anyhow::bail!("invalid IPv6 CONNECT target: {}", target);
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
async fn send_error(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    message: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status, message
    );
    writer
        .write_all(response.as_bytes())
        .await
        .context("failed to write error response")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
