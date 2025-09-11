use tokio::net::UnixListener;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::fs;
use std::path::Path;
use std::collections::HashMap;
use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tokio::process::Command as TokioCommand;

static GATEWAY_PROCS: Lazy<Mutex<HashMap<String, tokio::process::Child>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub async fn init_pod_listener(pod_name: &str) -> Result<String, String> {
    let dir = "/run/podmesh";
    if let Err(e) = fs::create_dir_all(dir).await {
        return Err(format!("failed to create dir {}: {}", dir, e));
    }
    let socket_path = format!("{}/raft_{}.sock", dir, pod_name);
    let path = Path::new(&socket_path);
    if path.exists() {
        let _ = fs::remove_file(path).await;
    }

    match UnixListener::bind(&socket_path) {
        Ok(listener) => {
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _addr)) => {
                            tokio::spawn(async move {
                                let (r, mut w) = stream.into_split();
                                let mut reader = BufReader::new(r);
                                let mut line = String::new();
                                loop {
                                    line.clear();
                                    match reader.read_line(&mut line).await {
                                        Ok(0) => break,
                                        Ok(_) => {
                                            let msg = line.trim();
                                            if msg.is_empty() { continue; }
                                            println!("[raft-socket] recv: {}", msg);
                                            let ack = serde_json::json!({"status":"ok","received": msg});
                                            let s = ack.to_string();
                                            if let Err(e) = w.write_all(s.as_bytes()).await {
                                                eprintln!("write error: {}", e);
                                                break;
                                            }
                                            if let Err(e) = w.write_all(b"\n").await {
                                                eprintln!("write error: {}", e);
                                                break;
                                            }
                                        }
                                        Err(e) => { eprintln!("read error: {}", e); break; }
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            eprintln!("accept error: {}", e);
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
            });
            Ok(socket_path)
        }
        Err(e) => Err(format!("failed to bind socket: {}", e)),
    }
}

/// Start a gateway binary for the given pod, passing --host_socket to it.
pub async fn start_gateway_for_pod(pod_name: &str, gateway_bin: Option<&str>, host_socket: &str) -> Result<(), String> {
    let bin = if let Some(b) = gateway_bin { b.to_string() } else { "../target/debug/gateway".to_string() };

    // spawn gateway process
    let mut cmd = TokioCommand::new(bin.clone());
    cmd.arg("--host-socket").arg(host_socket);
    // let the gateway use defaults for id and http_addr unless overridden

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
        let socket_path = format!("/run/podmesh/raft_{}.sock", pod_name);
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
