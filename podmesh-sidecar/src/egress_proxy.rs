//! Transparent egress proxy listener
//!
//! Listens for connections redirected by nftables REDIRECT rules,
//! recovers the original destination using SO_ORIGINAL_DST, and
//! tunnels the traffic through libp2p to the proxy node.

use anyhow::{Context, Result};
use libp2p::PeerId;
use protocol::egress::EgressProtocol as Protocol;
use socket2::{Domain, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::AsRawFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Port where the transparent proxy listens for redirected traffic
pub const EGRESS_PROXY_PORT: u16 = 15001;

/// Maximum concurrent connections to handle
#[allow(dead_code)]
const MAX_CONCURRENT_CONNECTIONS: usize = 1024;

/// Buffer size for bidirectional copy
#[allow(dead_code)]
const BUFFER_SIZE: usize = 16 * 1024;

/// SO_ORIGINAL_DST socket option for recovering the original destination
/// from a REDIRECT'd connection.
#[cfg(target_os = "linux")]
const SO_ORIGINAL_DST: libc::c_int = 80;

/// IP_TRANSPARENT socket option value
#[cfg(target_os = "linux")]
const IP_TRANSPARENT: libc::c_int = 19;

/// Configuration for the egress proxy
#[derive(Debug, Clone)]
pub struct EgressProxyConfig {
    /// Port to listen on for redirected connections
    pub listen_port: u16,
    /// Proxy peer ID to tunnel traffic through
    pub proxy_peer_id: Option<PeerId>,
}

impl Default for EgressProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: EGRESS_PROXY_PORT,
            proxy_peer_id: None,
        }
    }
}

/// Recovers the original destination address from a redirected TCP connection.
///
/// Uses the SO_ORIGINAL_DST socket option to get the address that the client
/// originally tried to connect to before nftables redirected the connection.
#[cfg(target_os = "linux")]
pub fn get_original_dst(stream: &TcpStream) -> Result<SocketAddr> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    
    // IPv4 original destination
    let mut addr: MaybeUninit<libc::sockaddr_in> = MaybeUninit::uninit();
    let mut len: libc::socklen_t = size_of::<libc::sockaddr_in>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            addr.as_mut_ptr() as *mut libc::c_void,
            &mut len,
        )
    };

    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow::anyhow!(
            "getsockopt(SO_ORIGINAL_DST) failed: {}",
            err
        ));
    }

    let addr = unsafe { addr.assume_init() };
    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);

    Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

#[cfg(not(target_os = "linux"))]
pub fn get_original_dst(_stream: &TcpStream) -> Result<SocketAddr> {
    Err(anyhow::anyhow!(
        "SO_ORIGINAL_DST not supported on this platform"
    ))
}

/// Message sent to request a tunnel connection
#[derive(Debug)]
pub struct TunnelRequest {
    /// Original destination host
    pub dest_host: String,
    /// Original destination port
    pub dest_port: u16,
    /// Protocol (TCP/UDP)
    pub protocol: Protocol,
    /// Client stream to tunnel
    pub client_stream: TcpStream,
    /// Whether to send HTTP 200 response when tunnel is established
    /// (used for HTTP CONNECT proxy mode)
    pub send_http_200: bool,
    /// Initial data to send to the destination after tunnel is established
    /// (used for plain HTTP proxy mode to forward the request)
    pub initial_data: Option<Vec<u8>>,
}

/// Egress proxy that handles redirected connections
pub struct EgressProxy {
    config: EgressProxyConfig,
    /// Channel to send tunnel requests to the main event loop
    tunnel_tx: mpsc::Sender<TunnelRequest>,
}

impl EgressProxy {
    /// Creates a new egress proxy
    pub fn new(
        config: EgressProxyConfig,
        tunnel_tx: mpsc::Sender<TunnelRequest>,
    ) -> Self {
        Self { config, tunnel_tx }
    }

    /// Starts the transparent proxy listener
    ///
    /// This function runs indefinitely, accepting connections on the proxy port
    /// and forwarding them to the tunnel channel for processing.
    pub async fn run(&self) -> Result<()> {
        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.listen_port));
        
        // Create the listener socket with transparent proxy options
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None)
            .context("Failed to create socket")?;
        
        // Enable IP_TRANSPARENT to accept redirected connections
        #[cfg(target_os = "linux")]
        {
            let enabled: libc::c_int = 1;
            let ret = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_IP,
                    IP_TRANSPARENT,
                    &enabled as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                log::warn!("Failed to set IP_TRANSPARENT (may not be required): {}", err);
                // Don't fail - IP_TRANSPARENT may not be needed for REDIRECT
            }
        }
        
        socket.set_reuse_address(true)?;
        socket.bind(&addr.into())?;
        socket.listen(128)?;
        socket.set_nonblocking(true)?;

        let listener = TcpListener::from_std(socket.into())
            .context("Failed to convert to TcpListener")?;

        log::info!(
            "Egress proxy listening on {} for redirected connections",
            addr
        );

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let tunnel_tx = self.tunnel_tx.clone();
                    
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, peer_addr, tunnel_tx).await {
                            log::error!("Failed to handle egress connection from {}: {}", peer_addr, e);
                        }
                    });
                }
                Err(e) => {
                    log::error!("Failed to accept connection: {}", e);
                }
            }
        }
    }
}

/// Handles an incoming redirected connection
async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    tunnel_tx: mpsc::Sender<TunnelRequest>,
) -> Result<()> {
    // Get the original destination before the redirect
    let orig_dst = get_original_dst(&stream)
        .context("Failed to get original destination")?;

    log::debug!(
        "Egress connection from {} -> original dest {}",
        peer_addr,
        orig_dst
    );

    // Send the tunnel request to the main event loop
    let request = TunnelRequest {
        dest_host: orig_dst.ip().to_string(),
        dest_port: orig_dst.port(),
        protocol: Protocol::Tcp,
        client_stream: stream,
        send_http_200: false, // Transparent proxy doesn't need HTTP response
        initial_data: None,
    };

    tunnel_tx
        .send(request)
        .await
        .context("Failed to send tunnel request")?;

    Ok(())
}

/// Performs bidirectional copy between two streams
pub async fn bidirectional_copy<A, B>(mut a: A, mut b: B) -> Result<(u64, u64)>
where
    A: AsyncReadExt + AsyncWriteExt + Unpin,
    B: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(&mut a);
    let (mut b_read, mut b_write) = tokio::io::split(&mut b);

    let a_to_b = tokio::io::copy(&mut a_read, &mut b_write);
    let b_to_a = tokio::io::copy(&mut b_read, &mut a_write);

    let (a_to_b_result, b_to_a_result) = tokio::join!(a_to_b, b_to_a);

    let a_to_b_bytes = a_to_b_result.unwrap_or(0);
    let b_to_a_bytes = b_to_a_result.unwrap_or(0);

    Ok((a_to_b_bytes, b_to_a_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = EgressProxyConfig::default();
        assert_eq!(config.listen_port, EGRESS_PROXY_PORT);
        assert!(config.proxy_peer_id.is_none());
    }
}
