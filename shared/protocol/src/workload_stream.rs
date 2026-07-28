use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub const WORKLOAD_ALPN: &[u8] = b"/podmesh/workload/1";
pub const MESH_DOMAIN_SUFFIX: &str = "mesh.local";
pub const WORKLOAD_FRAME_HEADER_BYTES: usize = 5;
pub const DEFAULT_WORKLOAD_STREAM_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_HANDSHAKE_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_REGISTRATION_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_PROXY_DISCOVERY_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_INGRESS_PAYLOAD_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_EGRESS_CONTROL_PAYLOAD_BYTES: usize = 32 * 1024;
pub const MAX_PROXY_ANNOUNCEMENT_PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkloadStreamKind {
    Handshake = 1,
    Registration = 2,
    ProxyDiscovery = 3,
    Ingress = 4,
    Egress = 5,
    ProxyAnnouncement = 6,
}

impl WorkloadStreamKind {
    pub const fn payload_limit(self) -> usize {
        match self {
            Self::Handshake => MAX_HANDSHAKE_PAYLOAD_BYTES,
            Self::Registration => MAX_REGISTRATION_PAYLOAD_BYTES,
            Self::ProxyDiscovery => MAX_PROXY_DISCOVERY_PAYLOAD_BYTES,
            Self::Ingress => MAX_INGRESS_PAYLOAD_BYTES,
            Self::Egress => MAX_EGRESS_CONTROL_PAYLOAD_BYTES,
            Self::ProxyAnnouncement => MAX_PROXY_ANNOUNCEMENT_PAYLOAD_BYTES,
        }
    }
}

impl TryFrom<u8> for WorkloadStreamKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Handshake),
            2 => Ok(Self::Registration),
            3 => Ok(Self::ProxyDiscovery),
            4 => Ok(Self::Ingress),
            5 => Ok(Self::Egress),
            6 => Ok(Self::ProxyAnnouncement),
            _ => bail!("unknown workload stream kind"),
        }
    }
}

pub async fn write_workload_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: WorkloadStreamKind,
    payload: &[u8],
    timeout_duration: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    ensure!(
        !payload.is_empty() && payload.len() <= kind.payload_limit(),
        "workload frame payload size is invalid"
    );
    let payload_len = u32::try_from(payload.len()).context("workload frame length exceeds u32")?;
    let mut header = [0u8; WORKLOAD_FRAME_HEADER_BYTES];
    header[0] = kind as u8;
    header[1..].copy_from_slice(&payload_len.to_be_bytes());

    tokio::select! {
        _ = cancellation.cancelled() => bail!("workload frame write cancelled"),
        result = tokio::time::timeout(timeout_duration, async {
            writer.write_all(&header).await?;
            writer.write_all(payload).await?;
            writer.flush().await
        }) => {
            result.context("workload frame write timed out")?.context("write workload frame")?;
        }
    }
    Ok(())
}

pub async fn read_workload_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    timeout_duration: Duration,
    cancellation: &CancellationToken,
) -> Result<(WorkloadStreamKind, Vec<u8>)> {
    tokio::select! {
        _ = cancellation.cancelled() => bail!("workload frame read cancelled"),
        result = tokio::time::timeout(timeout_duration, read_frame_inner(reader)) => {
            result.context("workload frame read timed out")?
        }
    }
}

async fn read_frame_inner<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(WorkloadStreamKind, Vec<u8>)> {
    let mut header = [0u8; WORKLOAD_FRAME_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .await
        .context("read workload frame header")?;
    let kind = WorkloadStreamKind::try_from(header[0])?;
    let payload_len = usize::try_from(u32::from_be_bytes(
        header[1..]
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid workload frame header"))?,
    ))
    .context("convert workload frame length")?;
    ensure!(
        payload_len > 0 && payload_len <= kind.payload_limit(),
        "workload frame declared size is invalid"
    );
    let mut payload = vec![0u8; payload_len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read workload frame payload")?;
    Ok((kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn every_kind_roundtrips() {
        for kind in [
            WorkloadStreamKind::Handshake,
            WorkloadStreamKind::Registration,
            WorkloadStreamKind::ProxyDiscovery,
            WorkloadStreamKind::Ingress,
            WorkloadStreamKind::Egress,
            WorkloadStreamKind::ProxyAnnouncement,
        ] {
            let (mut writer, mut reader) = tokio::io::duplex(64);
            let cancellation = CancellationToken::new();
            let (write_result, read_result) = tokio::join!(
                write_workload_frame(&mut writer, kind, b"payload", TEST_TIMEOUT, &cancellation,),
                read_workload_frame(&mut reader, TEST_TIMEOUT, &cancellation),
            );
            write_result.unwrap();
            assert_eq!(read_result.unwrap(), (kind, b"payload".to_vec()));
        }
    }

    #[tokio::test]
    async fn oversized_declared_payload_is_rejected_before_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let declared = u32::try_from(MAX_EGRESS_CONTROL_PAYLOAD_BYTES + 1).unwrap();
        writer
            .write_all(&[WorkloadStreamKind::Egress as u8])
            .await
            .unwrap();
        writer.write_all(&declared.to_be_bytes()).await.unwrap();
        assert!(
            read_workload_frame(&mut reader, TEST_TIMEOUT, &CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn truncated_payload_is_rejected() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(&[WorkloadStreamKind::Handshake as u8])
            .await
            .unwrap();
        writer.write_all(&10u32.to_be_bytes()).await.unwrap();
        writer.write_all(&[1, 2]).await.unwrap();
        drop(writer);
        assert!(
            read_workload_frame(&mut reader, TEST_TIMEOUT, &CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_stops_blocked_io() {
        let (_writer, mut reader) = tokio::io::duplex(64);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            read_workload_frame(&mut reader, TEST_TIMEOUT, &cancellation)
                .await
                .is_err()
        );
    }
}
