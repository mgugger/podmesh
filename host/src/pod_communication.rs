use tokio::fs;
use std::path::Path;
use std::collections::HashMap;
use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tokio::process::Command as TokioCommand;
use serde_json;
use tokio::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use libp2p::PeerId;
use tokio::sync::mpsc;

static GATEWAY_PROCS: Lazy<Mutex<HashMap<String, tokio::process::Child>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub async fn init_pod_listener(pod_name: &str) -> Result<String, String> {
    let dir = "/run/beemesh";
    if let Err(e) = fs::create_dir_all(dir).await {
        return Err(format!("failed to create dir {}: {}", dir, e));
    }
    let socket_path = format!("{}/gateway_{}.sock", dir, pod_name);
    let path = Path::new(&socket_path);
    if path.exists() {
        let _ = fs::remove_file(path).await;
    }

    // Do not bind the socket here. The host only creates the path and the gateway will bind it.
    Ok(socket_path)
}

/// Send a health check request to the gateway for the named pod over the unix socket.
pub async fn send_health_check(pod_name: &str) -> Result<bool, String> {
    let socket_path = format!("/run/beemesh/gateway_{}.sock", pod_name);

    // Connect to the gateway UDS socket and send the simple text-based request
    // the gateway expects (a newline-terminated path).
    let mut stream = UnixStream::connect(&socket_path).await
        .map_err(|e| format!("failed to connect to gateway socket {}: {}", socket_path, e))?;

    // Send the request as a single line. The gateway expects `/health` or `GET /health`.
    if let Err(e) = stream.write_all(b"/health\n").await {
        return Err(format!("failed to send health request: {}", e));
    }

    // Read 4-byte big-endian length prefix
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await
        .map_err(|e| format!("failed to read response length: {}", e))?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len == 0 {
        return Err("received empty response from gateway".to_string());
    }

    // Read the FlatBuffer payload
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await
        .map_err(|e| format!("failed to read response payload: {}", e))?;

    // Parse the FlatBuffer health message using beemesh-protocol's generated parser.
    match beemesh_protocol::flatbuffer::root_as_health(&payload) {
        Ok(health) => Ok(health.ok()),
        Err(e) => Err(format!("failed to parse flatbuffer health: {:?}", e)),
    }
}

/// Testing only: Start a gateway binary for the given pod, passing --host_socket to it.
pub async fn start_gateway_for_pod(pod_name: &str, gateway_bin: Option<&str>, host_socket: &str) -> Result<(), String> {
    let bin = if let Some(b) = gateway_bin { b.to_string() } else { "../target/debug/gateway".to_string() };

    // spawn gateway process
    let mut cmd = TokioCommand::new(bin.clone());
    cmd.env("RUST_LOG", "info");
    cmd.arg("--host-socket").arg(host_socket);
    // let the gateway use defaults for id and http_addr unless overridden

    // Pass the socket path as the gateway's http listen address so the gateway will bind it.
    cmd.arg("--socket-addr").arg(host_socket);

    match cmd.spawn() {
        Ok(child) => {
            let mut map = GATEWAY_PROCS.lock().await;
            map.insert(pod_name.to_string(), child);
            Ok(())
        }
        Err(e) => Err(format!("failed to spawn gateway {}: {}", bin, e)),
    }
}

/// Testing only: Stop a gateway process previously started for the pod.
pub async fn stop_gateway_for_pod(pod_name: &str) -> Result<(), String> {
    let mut map = GATEWAY_PROCS.lock().await;
    if let Some(mut child) = map.remove(pod_name) {
        if let Err(e) = child.kill().await {
            return Err(format!("failed to kill gateway process: {}", e));
        }
        // wait for it to exit
        let _ = child.wait().await;
        // attempt to remove the unix socket for this pod
        let socket_path = format!("/run/beemesh/_{}.sock", pod_name);
        match fs::remove_file(&socket_path).await {
            Ok(_) => {
                println!("removed socket {}", socket_path);
            }
            Err(e) => {
                // non-fatal; just log
                eprintln!("failed to remove socket {}: {}", socket_path, e);
            }
        }
        Ok(())
    } else {
        Err("no gateway process for pod".to_string())
    }
}

/// Send the manifest to a peer using libp2p request-response protocol.
/// This function sends an apply request FlatBuffer to the specified peer and waits for a response.
pub async fn send_apply_to_peer(
    peer: &str, 
    manifest: &serde_json::Value,
    control_tx: &mpsc::UnboundedSender<crate::libp2p_beemesh::Libp2pControl>
) -> Result<(), String> {
    println!("send_apply_to_peer: sending manifest to peer {}: {}", peer, manifest);
    
    // Parse the peer string into a PeerId
    let peer_id: PeerId = peer.parse()
        .map_err(|e| format!("invalid peer ID '{}': {}", peer, e))?;
    
    // Create a channel to receive the response
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    
    // Send the apply request via libp2p
    let control_msg = crate::libp2p_beemesh::Libp2pControl::SendApplyRequest {
        peer_id,
        manifest: manifest.clone(),
        reply_tx,
    };
    
    control_tx.send(control_msg)
        .map_err(|e| format!("failed to send control message: {}", e))?;
    
    // Wait for the response with a timeout
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reply_rx.recv()
    ).await
    .map_err(|_| "timeout waiting for apply response".to_string())?
    .ok_or_else(|| "control channel closed".to_string())?;
    
    match response {
        Ok(msg) => {
            println!("send_apply_to_peer: success - {}", msg);
            Ok(())
        }
        Err(err) => {
            println!("send_apply_to_peer: error - {}", err);
            Err(err)
        }
    }
}
