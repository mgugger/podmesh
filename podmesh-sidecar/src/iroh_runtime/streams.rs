use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, ProxyHttpRequest, ProxyHttpResponse, WorkloadStreamKind,
    read_workload_frame, write_workload_frame,
};
use reqwest::{
    Client, Method,
    header::{HeaderName, HeaderValue},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

use crate::{SidecarConfig, SidecarEvent, egress_proxy::TunnelRequest};

const MAX_LOCAL_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_EGRESS_INITIAL_DATA_BYTES: usize = 64 * 1024;

pub async fn serve_connection(
    connection: Connection,
    config: Arc<SidecarConfig>,
    http_client: Client,
    stream_slots: Arc<Semaphore>,
    cancellation: CancellationToken,
    disconnected_tx: mpsc::Sender<EndpointId>,
) {
    let remote = connection.remote_id();
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = connection.closed() => break,
            stream = connection.accept_bi() => match stream {
                Ok((send, recv)) => {
                    let config = config.clone();
                    let client = http_client.clone();
                    let slots = stream_slots.clone();
                    let cancellation = cancellation.clone();
                    tokio::spawn(async move {
                        let permit = tokio::time::timeout(
                            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                            slots.acquire_owned(),
                        ).await;
                        let Ok(Ok(_permit)) = permit else { return };
                        if let Err(error) = handle_incoming(send, recv, &config, client, &cancellation).await {
                            log::warn!("sidecar ingress stream rejected: {error}");
                        }
                    });
                }
                Err(error) => {
                    log::debug!("proxy connection ended endpoint={} error={error}", remote.fmt_short());
                    break;
                }
            }
        }
    }
    let _ = disconnected_tx.send(remote).await;
}

async fn handle_incoming(
    mut send: SendStream,
    mut recv: RecvStream,
    config: &SidecarConfig,
    http_client: Client,
    cancellation: &CancellationToken,
) -> Result<()> {
    let (kind, payload) =
        read_workload_frame(&mut recv, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    ensure!(
        kind == WorkloadStreamKind::Ingress,
        "sidecar accepts only ingress streams"
    );
    let mut request: ProxyHttpRequest =
        postcard::from_bytes(&payload).context("decode ingress request")?;
    if request.target_port == 0 {
        request.target_port = config.app_port;
    }
    let response = execute_local_http_request(http_client, request).await;
    let payload = postcard::to_allocvec(&response).context("serialize ingress response")?;
    write_workload_frame(
        &mut send,
        WorkloadStreamKind::Ingress,
        &payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        cancellation,
    )
    .await?;
    send.finish().context("finish ingress response")?;
    Ok(())
}

async fn execute_local_http_request(
    client: Client,
    request: ProxyHttpRequest,
) -> ProxyHttpResponse {
    let result = async {
        let method =
            Method::from_bytes(request.method.as_bytes()).context("invalid HTTP method")?;
        let mut path = if request.path_and_query.is_empty() {
            "/".to_string()
        } else {
            request.path_and_query.clone()
        };
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        let url = format!("http://127.0.0.1:{}{}", request.target_port, path);
        let mut builder = client.request(method, url);
        for (name, value) in request.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                builder = builder.header(name, value);
            }
        }
        let response = builder.body(request.body).send().await?;
        if let Some(length) = response.content_length() {
            ensure!(
                length <= MAX_LOCAL_HTTP_BODY_BYTES as u64,
                "local application response body exceeds limit"
            );
        }
        let status_code = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect();
        let body = response.bytes().await?;
        ensure!(
            body.len() <= MAX_LOCAL_HTTP_BODY_BYTES,
            "local application response body exceeds limit"
        );
        Ok::<ProxyHttpResponse, anyhow::Error>(ProxyHttpResponse {
            status_code,
            headers,
            body: body.to_vec(),
        })
    }
    .await;
    result.unwrap_or_else(|error| ProxyHttpResponse {
        status_code: 502,
        headers: vec![("x-podmesh-error".into(), error.to_string())],
        body: Vec::new(),
    })
}

pub async fn open_egress(
    connection: Connection,
    tunnel: TunnelRequest,
    cancellation: CancellationToken,
    event_tx: Option<mpsc::UnboundedSender<SidecarEvent>>,
) -> Result<()> {
    let destination_host = tunnel.dest_host.clone();
    let destination_port = tunnel.dest_port;
    let result = open_egress_inner(connection, tunnel, &cancellation).await;
    let event = match &result {
        Ok(()) => SidecarEvent::EgressTunnelEstablished {
            dest_host: destination_host,
            dest_port: destination_port,
        },
        Err(error) => SidecarEvent::EgressTunnelFailed {
            dest_host: destination_host,
            dest_port: destination_port,
            error: error.to_string(),
        },
    };
    if let Some(sender) = event_tx {
        let _ = sender.send(event);
    }
    result
}

async fn open_egress_inner(
    connection: Connection,
    mut tunnel: TunnelRequest,
    cancellation: &CancellationToken,
) -> Result<()> {
    let (mut send, mut recv) =
        tokio::time::timeout(DEFAULT_WORKLOAD_STREAM_TIMEOUT, connection.open_bi())
            .await
            .context("egress stream open timed out")?
            .context("open egress stream")?;
    let request = EgressTunnelRequest::tcp(&tunnel.dest_host, tunnel.dest_port);
    let payload = postcard::to_allocvec(&request).context("serialize egress request")?;
    write_workload_frame(
        &mut send,
        WorkloadStreamKind::Egress,
        &payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        cancellation,
    )
    .await?;
    let (kind, payload) =
        read_workload_frame(&mut recv, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    ensure!(
        kind == WorkloadStreamKind::Egress,
        "unexpected egress response kind"
    );
    let response: EgressTunnelResponse =
        postcard::from_bytes(&payload).context("decode egress response")?;
    ensure!(
        response.success,
        "egress proxy rejected tunnel: {}",
        response.error.as_deref().unwrap_or("unknown error")
    );
    log::info!(
        "egress tunnel established destination={}:{}",
        tunnel.dest_host,
        tunnel.dest_port
    );
    if tunnel.send_http_200 {
        tunnel
            .client_stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    }
    if let Some(initial_data) = tunnel.initial_data.take() {
        ensure!(
            initial_data.len() <= MAX_EGRESS_INITIAL_DATA_BYTES,
            "egress initial data exceeds limit"
        );
        send.write_all(&initial_data).await?;
    }
    let (mut client_read, mut client_write) = tunnel.client_stream.into_split();
    let upload = async {
        let bytes = tokio::io::copy(&mut client_read, &mut send).await?;
        send.finish()?;
        Ok::<u64, anyhow::Error>(bytes)
    };
    let download = async {
        let bytes = tokio::io::copy(&mut recv, &mut client_write).await?;
        client_write.shutdown().await?;
        Ok::<u64, anyhow::Error>(bytes)
    };
    let _ = tokio::try_join!(upload, download)?;
    Ok(())
}
