//! Bounded raw HTTP body framing after authorized ingress setup.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub const HTTP_BODY_CHUNK_HEADER_BYTES: usize = 4;
pub const MAX_HTTP_BODY_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HttpBodyProgress {
    total_bytes: usize,
}

impl HttpBodyProgress {
    pub fn total_bytes(self) -> usize {
        self.total_bytes
    }

    fn add(&mut self, bytes: usize) -> Result<()> {
        let total = self
            .total_bytes
            .checked_add(bytes)
            .context("HTTP body byte count overflow")?;
        ensure!(total <= MAX_HTTP_BODY_BYTES, "HTTP body exceeds byte limit");
        self.total_bytes = total;
        Ok(())
    }
}

pub async fn read_http_body_chunk<R: AsyncRead + Unpin>(
    reader: &mut R,
    progress: &mut HttpBodyProgress,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; HTTP_BODY_CHUNK_HEADER_BYTES];
    read_exact(
        reader,
        &mut header,
        timeout,
        cancellation,
        "HTTP body chunk header",
    )
    .await?;
    let length =
        usize::try_from(u32::from_be_bytes(header)).context("convert HTTP chunk length")?;
    if length == 0 {
        return Ok(None);
    }
    ensure!(
        length <= MAX_HTTP_BODY_CHUNK_BYTES,
        "HTTP body chunk exceeds byte limit"
    );
    progress.add(length)?;
    let mut chunk = vec![0u8; length];
    read_exact(reader, &mut chunk, timeout, cancellation, "HTTP body chunk").await?;
    Ok(Some(chunk))
}

pub async fn write_http_body_chunk<W: AsyncWrite + Unpin>(
    writer: &mut W,
    chunk: &[u8],
    progress: &mut HttpBodyProgress,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    ensure!(
        !chunk.is_empty(),
        "empty HTTP body chunk is reserved as terminator"
    );
    for part in chunk.chunks(MAX_HTTP_BODY_CHUNK_BYTES) {
        progress.add(part.len())?;
        let length = u32::try_from(part.len()).context("convert HTTP body chunk length")?;
        write_all(
            writer,
            &length.to_be_bytes(),
            timeout,
            cancellation,
            "HTTP body chunk header",
        )
        .await?;
        write_all(writer, part, timeout, cancellation, "HTTP body chunk").await?;
    }
    Ok(())
}

pub async fn finish_http_body<W: AsyncWrite + Unpin>(
    writer: &mut W,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all(
        writer,
        &0u32.to_be_bytes(),
        timeout,
        cancellation,
        "HTTP body terminator",
    )
    .await?;
    tokio::select! {
        _ = cancellation.cancelled() => bail!("HTTP body flush cancelled"),
        result = tokio::time::timeout(timeout, writer.flush()) => {
            result.context("HTTP body flush timed out")?.context("flush HTTP body")?;
        }
    }
    Ok(())
}

async fn read_exact<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
    timeout: Duration,
    cancellation: &CancellationToken,
    what: &str,
) -> Result<()> {
    tokio::select! {
        _ = cancellation.cancelled() => bail!("{what} read cancelled"),
        result = tokio::time::timeout(timeout, reader.read_exact(buffer)) => {
            result.with_context(|| format!("{what} read timed out"))?
                .with_context(|| format!("read {what}"))?;
        }
    }
    Ok(())
}

async fn write_all<W: AsyncWrite + Unpin>(
    writer: &mut W,
    buffer: &[u8],
    timeout: Duration,
    cancellation: &CancellationToken,
    what: &str,
) -> Result<()> {
    tokio::select! {
        _ = cancellation.cancelled() => bail!("{what} write cancelled"),
        result = tokio::time::timeout(timeout, writer.write_all(buffer)) => {
            result.with_context(|| format!("{what} write timed out"))?
                .with_context(|| format!("write {what}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn chunks_and_terminator_roundtrip() {
        let (mut writer, mut reader) = tokio::io::duplex(MAX_HTTP_BODY_CHUNK_BYTES * 2);
        let cancellation = CancellationToken::new();
        let body = vec![7u8; MAX_HTTP_BODY_CHUNK_BYTES + 3];
        let write = async {
            let mut progress = HttpBodyProgress::default();
            write_http_body_chunk(
                &mut writer,
                &body,
                &mut progress,
                TEST_TIMEOUT,
                &cancellation,
            )
            .await?;
            finish_http_body(&mut writer, TEST_TIMEOUT, &cancellation).await?;
            Ok::<usize, anyhow::Error>(progress.total_bytes())
        };
        let read = async {
            let mut progress = HttpBodyProgress::default();
            let mut received = Vec::new();
            while let Some(chunk) =
                read_http_body_chunk(&mut reader, &mut progress, TEST_TIMEOUT, &cancellation)
                    .await?
            {
                received.extend_from_slice(&chunk);
            }
            Ok::<(Vec<u8>, usize), anyhow::Error>((received, progress.total_bytes()))
        };
        let (written, read) = tokio::join!(write, read);
        assert_eq!(written.unwrap(), body.len());
        assert_eq!(read.unwrap(), (body, MAX_HTTP_BODY_CHUNK_BYTES + 3));
    }

    #[tokio::test]
    async fn oversized_declaration_is_refused_before_body_read() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(
                &u32::try_from(MAX_HTTP_BODY_CHUNK_BYTES + 1)
                    .unwrap()
                    .to_be_bytes(),
            )
            .await
            .unwrap();
        let mut progress = HttpBodyProgress::default();
        assert!(
            read_http_body_chunk(
                &mut reader,
                &mut progress,
                TEST_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
        assert_eq!(progress.total_bytes(), 0);
    }

    #[tokio::test]
    async fn cumulative_limit_is_checked_before_write() {
        let (mut writer, _reader) = tokio::io::duplex(16);
        let mut progress = HttpBodyProgress {
            total_bytes: MAX_HTTP_BODY_BYTES,
        };
        assert!(
            write_http_body_chunk(
                &mut writer,
                b"x",
                &mut progress,
                TEST_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn truncated_chunk_is_refused() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_all(&4u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"xy").await.unwrap();
        drop(writer);
        let mut progress = HttpBodyProgress::default();
        assert!(
            read_http_body_chunk(
                &mut reader,
                &mut progress,
                TEST_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn cumulative_limit_is_checked_before_chunk_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_all(&1u32.to_be_bytes()).await.unwrap();
        let mut progress = HttpBodyProgress {
            total_bytes: MAX_HTTP_BODY_BYTES,
        };
        assert!(
            read_http_body_chunk(
                &mut reader,
                &mut progress,
                TEST_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
    }
}
