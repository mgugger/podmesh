use clap::Parser;
use tracing_subscriber::EnvFilter;

mod gw_raft;

#[derive(Parser, Clone, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Opt {
    #[clap(long, default_value = "1")]
    pub id: u64,

    #[clap(long, default_value = "/run/beemesh/gateway_testpod.sock")]
    pub socket_addr: String,

    #[clap(long, default_value = "/run/beemesh/host.sock")]
    pub host_socket: String,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_thread_ids(true)
        .with_level(true)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let options = Opt::parse();

    #[cfg(feature = "raft")]
    let app_state = gw_raft::start_example_raft_node(options.id, options.socket_addr.clone(), options.host_socket).await?;

    if !options.socket_addr.is_empty() && options.socket_addr.starts_with('/') {
        // Remove stale socket if present
        let _ = std::fs::remove_file(&options.socket_addr);
        let listener = match tokio::net::UnixListener::bind(&options.socket_addr) {
            Ok(l) => l,
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("failed to bind UDS: {}", e))),
        };

        tracing::info!(socket=%options.socket_addr, "gateway listening on UDS");

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    // Spawn a task to handle this connection.
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

                        let (r, mut w) = tokio::io::split(stream);
                        let mut reader = BufReader::new(r);
                        let mut req_buf: Vec<u8> = Vec::new();

                        // Read a single newline-terminated request from the client.
                        match reader.read_until(b'\n', &mut req_buf).await {
                            Ok(0) => {
                                tracing::debug!("connection closed by peer");
                                return;
                            }
                            Ok(_) => {
                                let req = String::from_utf8_lossy(&req_buf);
                                let req_trim = req.trim();
                                tracing::debug!(request=%req_trim, "received request");

                                // Accept either `/health` or `GET /health` for compatibility with
                                // simple non-HTTP clients that only send the path or a "GET /health" token.
                                if req_trim == "/health" || req_trim == "GET /health" {
                                    // Build the FlatBuffer health message using helper from protocol crate.
                                    let resp = beemesh_protocol::machine::build_health(true, "healthy");
                                    let len_be = (resp.len() as u32).to_be_bytes();

                                    if let Err(e) = w.write_all(&len_be).await {
                                        tracing::error!(error=%e, "failed to write response length");
                                        return;
                                    }
                                    if let Err(e) = w.write_all(&resp).await {
                                        tracing::error!(error=%e, "failed to write response payload");
                                        return;
                                    }

                                    // Flush to ensure client receives the bytes promptly.
                                    if let Err(e) = w.flush().await {
                                        tracing::error!(error=%e, "failed to flush response");
                                    }

                                    tracing::debug!(bytes=%resp.len(), "sent health response");
                                } else {
                                    // Unknown request: reply with zero-length payload (len=0)
                                    let zero: [u8; 4] = 0u32.to_be_bytes();
                                    if let Err(e) = w.write_all(&zero).await {
                                        tracing::error!(error=%e, "failed to write zero-length response");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(error=%e, "failed to read from connection");
                                return;
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(error=%e, "failed to accept incoming connection");
                    // small sleep to avoid tight loop on repeated accept errors
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    } else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "gateway expects UDS socket path in http_addr"));
    }
}