use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::IROH_ENDPOINT_ID_BYTES;

pub const FRAME_LENGTH_PREFIX_BYTES: usize = 4;
pub const MAX_TENANT_SESSION_FRAME_BYTES: usize = 32 * 1024;
pub const MAX_REGISTRATION_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_INGRESS_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EGRESS_FRAME_BYTES: usize = 32 * 1024;
pub const MAX_PROXY_DISCOVERY_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_IROH_FRAME_BYTES: usize = MAX_INGRESS_FRAME_BYTES;
pub const DEFAULT_IROH_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_TENANT_OWNER_LEN: usize = 128;
pub const MAX_WORKLOAD_ID_LEN: usize = 128;
pub const MAX_OPERATION_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_PROXY_GRANT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkloadPeerRole {
    Proxy,
    Sidecar,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantSessionFrame {
    pub tenant_owner: String,
    pub role: WorkloadPeerRole,
    #[serde(with = "serde_bytes")]
    pub subject_endpoint_id: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub audience_endpoint_id: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub proxy_grant: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub mesh_join_biscuit: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OperationKind {
    Registration,
    Ingress,
    Egress,
    ProxyDiscovery,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationFrame {
    pub kind: OperationKind,
    pub workload_id: String,
    #[serde(with = "serde_bytes")]
    pub biscuit: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IrohFrame {
    TenantSession(TenantSessionFrame),
    Operation(OperationFrame),
}

impl IrohFrame {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::TenantSession(frame) => frame.validate(),
            Self::Operation(frame) => frame.validate(),
        }
    }

    pub fn encoded_limit(&self) -> usize {
        match self {
            Self::TenantSession(_) => MAX_TENANT_SESSION_FRAME_BYTES,
            Self::Operation(frame) => frame.encoded_limit(),
        }
    }
}

impl TenantSessionFrame {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.tenant_owner.is_empty() && self.tenant_owner.len() <= MAX_TENANT_OWNER_LEN,
            "tenant owner length is invalid"
        );
        ensure!(
            self.subject_endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "session subject endpoint ID must contain 32 bytes"
        );
        ensure!(
            self.audience_endpoint_id.len() == IROH_ENDPOINT_ID_BYTES,
            "session audience endpoint ID must contain 32 bytes"
        );
        ensure!(
            self.proxy_grant.len() <= MAX_PROXY_GRANT_BYTES,
            "session proxy grant exceeds limit"
        );
        ensure!(
            !self.mesh_join_biscuit.is_empty()
                && self.mesh_join_biscuit.len() <= MAX_OPERATION_TOKEN_BYTES,
            "mesh-join Biscuit size is invalid"
        );
        Ok(())
    }
}

impl OperationFrame {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.workload_id.is_empty() && self.workload_id.len() <= MAX_WORKLOAD_ID_LEN,
            "operation workload ID length is invalid"
        );
        ensure!(
            !self.biscuit.is_empty() && self.biscuit.len() <= MAX_OPERATION_TOKEN_BYTES,
            "operation Biscuit size is invalid"
        );
        Ok(())
    }

    fn encoded_limit(&self) -> usize {
        match self.kind {
            OperationKind::Registration => MAX_REGISTRATION_FRAME_BYTES,
            OperationKind::Ingress => MAX_INGRESS_FRAME_BYTES,
            OperationKind::Egress => MAX_EGRESS_FRAME_BYTES,
            OperationKind::ProxyDiscovery => MAX_PROXY_DISCOVERY_FRAME_BYTES,
        }
    }
}

pub async fn write_iroh_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &IrohFrame,
    timeout_duration: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    frame.validate()?;
    let payload = postcard::to_allocvec(frame).context("serialize Iroh frame")?;
    ensure!(
        payload.len() <= frame.encoded_limit() && payload.len() <= MAX_IROH_FRAME_BYTES,
        "Iroh frame exceeds per-kind size limit"
    );
    let payload_len = u32::try_from(payload.len()).context("Iroh frame length exceeds u32")?;
    let mut encoded = Vec::with_capacity(FRAME_LENGTH_PREFIX_BYTES + payload.len());
    encoded.extend_from_slice(&payload_len.to_be_bytes());
    encoded.extend_from_slice(&payload);

    tokio::select! {
        _ = cancellation.cancelled() => bail!("Iroh frame write cancelled"),
        result = tokio::time::timeout(timeout_duration, writer.write_all(&encoded)) => {
            result.context("Iroh frame write timed out")?.context("write Iroh frame")?;
        }
    }
    Ok(())
}

pub async fn read_iroh_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    timeout_duration: Duration,
    cancellation: &CancellationToken,
) -> Result<IrohFrame> {
    tokio::select! {
        _ = cancellation.cancelled() => bail!("Iroh frame read cancelled"),
        result = tokio::time::timeout(timeout_duration, read_frame_inner(reader)) => {
            result.context("Iroh frame read timed out")?
        }
    }
}

async fn read_frame_inner<R: AsyncRead + Unpin>(reader: &mut R) -> Result<IrohFrame> {
    let mut prefix = [0u8; FRAME_LENGTH_PREFIX_BYTES];
    reader
        .read_exact(&mut prefix)
        .await
        .context("read Iroh frame length")?;
    let payload_len =
        usize::try_from(u32::from_be_bytes(prefix)).context("convert Iroh frame length")?;
    ensure!(
        payload_len > 0 && payload_len <= MAX_IROH_FRAME_BYTES,
        "Iroh frame declared size is invalid"
    );
    let mut payload = vec![0u8; payload_len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read Iroh frame payload")?;
    let frame: IrohFrame = postcard::from_bytes(&payload).context("decode Iroh frame")?;
    frame.validate()?;
    ensure!(
        payload_len <= frame.encoded_limit(),
        "Iroh frame exceeds decoded kind size limit"
    );
    Ok(frame)
}
