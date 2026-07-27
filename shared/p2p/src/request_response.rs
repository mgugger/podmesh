use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::request_response;

pub const MAX_REQUEST_RESPONSE_FRAME_BYTES: usize = 1024 * 1024;

/// Simple length-prefixed codec carrying raw bytes for request/response pairs.
#[derive(Debug, Clone, Default)]
pub struct ByteCodec;

// Common podmesh protocols all share the same byte codec.
pub type HandshakeCodec = ByteCodec;

#[async_trait::async_trait]
impl request_response::Codec for ByteCodec {
    type Protocol = &'static str;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_length_prefixed_bytes(io).await
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_length_prefixed_bytes(io).await
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_length_prefixed_bytes(io, &req).await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_length_prefixed_bytes(io, &res).await
    }
}

pub async fn read_length_prefixed_bytes<T>(io: &mut T) -> std::io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_REQUEST_RESPONSE_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request-response frame exceeds size limit",
        ));
    }

    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_length_prefixed_bytes<T>(io: &mut T, data: &[u8]) -> std::io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
{
    if data.len() > MAX_REQUEST_RESPONSE_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request-response frame exceeds size limit",
        ));
    }
    let len = u32::try_from(data.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request-response frame length is not representable",
        )
    })?;
    io.write_all(&len.to_be_bytes()).await?;
    io.write_all(data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_frame_before_payload_read() {
        let announced = (MAX_REQUEST_RESPONSE_FRAME_BYTES as u32 + 1).to_be_bytes();
        let mut input = futures::io::Cursor::new(announced.to_vec());
        let error =
            futures::executor::block_on(read_length_prefixed_bytes(&mut input)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
