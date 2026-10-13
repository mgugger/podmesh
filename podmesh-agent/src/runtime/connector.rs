//! Podman adapter for the runtime-neutral connector protocol.
//!
//! Locate the exact injected container in the owned pod and execute only the
//! connector binary's fixed adapter. No shell, arbitrary command, host path or
//! caller-selected container is exposed through the runtime contract.

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, ensure};
use protocol::workload_connector::{
    ConnectorRequest, ConnectorResponse, MAX_CONNECTOR_FRAME_BYTES, read_frame, write_frame,
};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use super::PodmanRuntime;

const RUNTIME_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const CONNECTOR_BINARY: &str = "/usr/local/bin/podmesh-sidecar";

impl PodmanRuntime {
    pub(super) async fn exchange_connector(
        &self,
        runtime_id: &str,
        request: ConnectorRequest,
    ) -> Result<ConnectorResponse> {
        request.validate()?;
        validate_runtime_id(runtime_id)?;
        let output = bounded_command(
            Command::new("podman").args([
                "pod",
                "inspect",
                "--format",
                "{{json .Containers}}",
                runtime_id,
            ]),
            None,
        )
        .await
        .context("locate workload connector")?;
        let container = select_connector_container(runtime_id, &output)?;
        let mut input = Vec::new();
        write_frame(&mut input, &request.to_bytes()?).await?;
        let output = bounded_command(
            Command::new("podman").args([
                "exec",
                "--interactive",
                &container,
                CONNECTOR_BINARY,
                "connector-control",
            ]),
            Some(input),
        )
        .await
        .context("exchange workload connector frame")?;
        let mut reader = output.as_slice();
        let response = ConnectorResponse::from_bytes(&read_frame(&mut reader).await?)?;
        ensure!(
            reader.is_empty(),
            "connector runtime returned trailing frames"
        );
        Ok(response)
    }
}

fn validate_runtime_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 253
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')),
        "invalid connector runtime identity"
    );
    ensure!(
        !value.starts_with('-'),
        "invalid connector runtime identity"
    );
    Ok(())
}

#[derive(Deserialize)]
struct PodContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
}

fn select_connector_container(runtime_id: &str, bytes: &[u8]) -> Result<String> {
    let containers: Vec<PodContainer> =
        serde_json::from_slice(bytes).context("decode owned pod containers")?;
    ensure!(
        containers.len() <= 64,
        "owned pod container count exceeds limit"
    );
    let expected = format!("{runtime_id}-{}", crate::sidecar::SIDECAR_NAME);
    let mut matching = containers
        .into_iter()
        .filter(|container| container.name == expected);
    let container = matching
        .next()
        .context("owned workload connector is unavailable")?;
    ensure!(
        matching.next().is_none(),
        "ambiguous workload connector container"
    );
    ensure!(
        container.id.len() == 64 && container.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid workload connector container id"
    );
    Ok(container.id)
}

async fn read_bounded(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= limit,
        "connector process output exceeds limit"
    );
    Ok(bytes)
}

/// Unlike general runtime output, both pipes are bounded while being read.
/// Diagnostics are drained but never reflected into a credential-bearing reply.
async fn bounded_command(command: &mut Command, input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    command
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("start connector runtime operation")?;
    let stdout = child.stdout.take().context("missing connector stdout")?;
    let stderr = child.stderr.take().context("missing connector stderr")?;
    let stdin = child.stdin.take();
    let result = tokio::time::timeout(RUNTIME_EXCHANGE_TIMEOUT, async {
        let input = async move {
            if let (Some(bytes), Some(mut stdin)) = (input, stdin) {
                stdin.write_all(&bytes).await?;
                stdin.shutdown().await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let wait = async { Ok::<_, anyhow::Error>(child.wait().await?) };
        let ((), output, _, status) = tokio::try_join!(
            input,
            read_bounded(stdout, MAX_CONNECTOR_FRAME_BYTES + 4),
            read_bounded(stderr, MAX_DIAGNOSTIC_BYTES),
            wait,
        )?;
        ensure!(status.success(), "connector runtime operation failed");
        Ok(output)
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(output),
        failure => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            match failure {
                Ok(Err(error)) => Err(error),
                Err(_) => anyhow::bail!("connector runtime operation timed out"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_exact_owned_container_not_similar_names() {
        let id = "a".repeat(64);
        let bytes = serde_json::to_vec(&serde_json::json!([
            {"Id": "b".repeat(64), "Name": "owned-pod-app-podmesh-sidecar"},
            {"Id": id, "Name": "owned-pod-podmesh-sidecar"}
        ]))
        .unwrap();
        assert_eq!(select_connector_container("owned-pod", &bytes).unwrap(), id);
        assert!(select_connector_container("other-pod", &bytes).is_err());
        assert!(
            select_connector_container(
                "owned-pod",
                br#"[{"Id":"--escape","Name":"owned-pod-podmesh-sidecar"}]"#
            )
            .is_err()
        );
        assert!(validate_runtime_id("--escape").is_err());
        assert!(validate_runtime_id("/host/path").is_err());
        assert!(validate_runtime_id("owned-pod").is_ok());
    }

    #[tokio::test]
    async fn process_io_bounds_are_enforced_while_reading() {
        assert!(read_bounded(&b"12345"[..], 4).await.is_err());
        assert_eq!(read_bounded(&b"1234"[..], 4).await.unwrap(), b"1234");
    }

    #[tokio::test]
    async fn subprocess_errors_do_not_reflect_diagnostics() {
        // Test helper only: production never invokes a shell.
        let error = bounded_command(
            Command::new("sh").args(["-c", "echo secret-token >&2; exit 1"]),
            None,
        )
        .await
        .unwrap_err();
        assert!(!format!("{error:#}").contains("secret-token"));
        let output = bounded_command(
            Command::new("sh").args(["-c", "cat"]),
            Some(b"bounded input".to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(output, b"bounded input");
    }
}
