//! Re-executes this Rust test binary in only the workload's network namespace.
//! No shell/package manager/probe binary is required in the scratch connector.
use super::*;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, RecordType},
};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};

pub(super) async fn probe(workload_id: &str, name: &str, port: u16) -> Result<()> {
    let pod = format!("{}-pod", workload_runtime_name(workload_id));
    let containers = run_podman_command([
        "ps",
        "--filter",
        &format!("pod={pod}"),
        "--format",
        "{{.ID}} {{.Names}}",
    ])
    .await?;
    let connector = containers
        .lines()
        .find(|line| line.contains("podmesh-sidecar"))
        .context("DNS probe connector missing")?
        .split_whitespace()
        .next()
        .context("DNS probe container ID missing")?;
    let pid = run_podman_command(["inspect", "--format", "{{.State.Pid}}", connector]).await?;
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        TokioCommand::new("podman")
            .args(["unshare", "nsenter", "--target", pid.trim(), "--net"])
            .arg(std::env::current_exe()?)
            .args(["--exact", "dns::namespace_probe", "--nocapture"])
            .env("PODMESH_NAMESPACE_DNS_PROBE", name)
            .env("PODMESH_NAMESPACE_SERVICE_PORT", port.to_string())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("namespace DNS/TCP probe timed out")??;
    if !output.status.success() {
        for container in capture_podman_containers().await? {
            for name in container
                .names
                .iter()
                .filter(|name| name.ends_with("podmesh-sidecar"))
            {
                let output = TokioCommand::new("podman")
                    .args(["logs", "--tail", "2000", name])
                    .kill_on_drop(true)
                    .output()
                    .await?;
                let logs = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                log::error!(
                    "DNS probe connector {name} diagnostics: {}",
                    logs.lines()
                        .filter(|line| line.contains("WARN") || line.contains("ERROR"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }
    anyhow::ensure!(
        output.status.success(),
        "workload namespace DNS/TCP probe failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    log::info!(
        "namespace DNS/TCP probe verified: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}

fn query(name: &str, kind: RecordType, tcp: bool) -> Result<Message> {
    let mut message = Message::new(4381, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message
        .queries
        .push(Query::query(Name::from_ascii(name)?, kind));
    let bytes = message.to_vec()?;
    let reply = if tcp {
        let mut stream =
            TcpStream::connect_timeout(&"127.0.0.1:53".parse()?, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        stream.write_all(&(bytes.len() as u16).to_be_bytes())?;
        stream.write_all(&bytes)?;
        let mut length = [0; 2];
        stream.read_exact(&mut length)?;
        let mut reply = vec![0; u16::from_be_bytes(length) as usize];
        stream.read_exact(&mut reply)?;
        reply
    } else {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.connect("127.0.0.1:53")?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.send(&bytes)?;
        let mut reply = vec![0; 4096];
        let size = socket.recv(&mut reply)?;
        reply.truncate(size);
        reply
    };
    let reply = Message::from_vec(&reply)?;
    anyhow::ensure!(
        reply.metadata.id == 4381 && reply.metadata.response_code == ResponseCode::NoError,
        "DNS lookup {name} failed: {:?}",
        reply.metadata.response_code
    );
    Ok(reply)
}

fn address(reply: &Message) -> Result<Ipv4Addr> {
    reply
        .answers
        .iter()
        .find_map(|answer| match &answer.data {
            RData::A(value) => Some(value.0),
            _ => None,
        })
        .context("DNS A answer missing")
}

#[test]
fn namespace_probe() -> Result<()> {
    let Ok(name) = std::env::var("PODMESH_NAMESPACE_DNS_PROBE") else {
        return Ok(());
    };
    let port = std::env::var("PODMESH_NAMESPACE_SERVICE_PORT")?.parse::<u16>()?;
    // Namespace UID 0 is the connector UID too: DNS/VIP rules must precede
    // UID and loopback exemptions, not silently bypass this application.
    let vip = address(&query(&name, RecordType::A, false)?)?;
    anyhow::ensure!(
        vip.octets()[..3] == [198, 18, 0],
        "DNS did not return a service VIP"
    );
    anyhow::ensure!(
        address(&query(&name, RecordType::A, true)?)? == vip,
        "UDP/TCP DNS disagree"
    );
    anyhow::ensure!(
        query(&name, RecordType::AAAA, false)?.answers.is_empty(),
        "IPv4-only service fabricated AAAA"
    );
    let runtime = address(&query("podmesh-control.", RecordType::A, false)?)?;
    anyhow::ensure!(
        runtime.octets()[..2] != [198, 18],
        "runtime DNS was hijacked by a service VIP"
    );
    for _ in 0..4 {
        let mut stream = TcpStream::connect_timeout(&(vip, port).into(), Duration::from_secs(10))?;
        // The dispatcher permits 30 seconds for bounded setup-only retries.
        stream.set_read_timeout(Some(Duration::from_secs(35)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        stream.write_all(b"GET / HTTP/1.1\r\nHost: internal-probe\r\nConnection: close\r\n\r\n")?;
        let mut response = Vec::new();
        stream
            .take(128 * 1024)
            .read_to_end(&mut response)
            .context("read remote VIP HTTP response")?;
        anyhow::ensure!(
            String::from_utf8_lossy(&response).contains(EXPECTED_BODY_SUBSTRING),
            "peer TCP response was not the remote nginx application"
        );
    }
    println!(
        "same-UID UDP/TCP split DNS, AAAA NODATA, runtime upstream bypass and remote VIP:{port} TCP passed"
    );
    Ok(())
}
