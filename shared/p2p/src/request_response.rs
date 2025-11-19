use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::request_response;

/// Simple length-prefixed codec carrying raw bytes for request/response pairs.
#[derive(Debug, Clone, Default)]
pub struct ByteCodec;

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

    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_length_prefixed_bytes<T>(io: &mut T, data: &[u8]) -> std::io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
{
    let len = data.len() as u32;
    io.write_all(&len.to_be_bytes()).await?;
    io.write_all(data).await?;
    Ok(())
}
