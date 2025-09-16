use tokio::fs;
use std::path::Path;
use std::collections::HashMap;
use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tokio::process::Command as TokioCommand;
use hyper::{Client, Body, Request, Method, StatusCode};
use hyperlocal::{UnixConnector, Uri};
use serde::Serialize;
use serde_json;
use tokio::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    match beemesh_protocol::generated::beemesh::root_as_health(&payload) {
        Ok(health) => Ok(health.ok()),
        Err(e) => Err(format!("failed to parse flatbuffer health: {:?}", e)),
    }
}

/// Generic request sender to the gateway unix socket for the given pod.
///
/// - `method` is an HTTP verb like "GET" or "POST".
/// - `path` is the request path (e.g. "/health" or "/init").
/// - `json_body` is an optional reference to a value that will be serialized as JSON for the request body.
///
/// Returns tuple (StatusCode, Option<serde_json::Value>, String) where the Option is the parsed
/// JSON response if parsing succeeds, and the String is the raw response body as text.
pub async fn send_request<T: Serialize>(
    pod_name: &str,
    method: &str,
    path: &str,
    json_body: Option<&T>,
) -> Result<(StatusCode, Option<serde_json::Value>, String), String> {
    let socket_path = format!("/run/beemesh/gateway_{}.sock", pod_name);
    let connector = UnixConnector;
    let client: Client<_, Body> = Client::builder().build(connector);

    let uri: Uri = Uri::new(socket_path.clone(), path).into();

    let m = Method::from_bytes(method.as_bytes()).map_err(|e| format!("invalid method {}: {}", method, e))?;

    let req = if let Some(body_val) = json_body {
        let v = serde_json::to_vec(body_val).map_err(|e| format!("json serialize error: {}", e))?;
        Request::builder()
            .method(m)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(v))
            .map_err(|e| format!("request build error: {}", e))?
    } else {
        Request::builder()
            .method(m)
            .uri(uri)
            .body(Body::empty())
            .map_err(|e| format!("request build error: {}", e))?
    };

    let resp = client.request(req).await.map_err(|e| format!("request failed: {}", e))?;
    let status = resp.status();
    let bytes = hyper::body::to_bytes(resp.into_body()).await.map_err(|e| format!("body read error: {}", e))?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let parsed = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(v) => Some(v),
        Err(_) => None,
    };

    Ok((status, parsed, text))
}

/// Start a gateway binary for the given pod, passing --host_socket to it.
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

/// Stop a gateway process previously started for the pod.
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
