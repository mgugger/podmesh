use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use protocol::egress::{EgressTunnelRequest, EgressTunnelResponse};
use protocol::{
    DEFAULT_WORKLOAD_STREAM_TIMEOUT, IngressRequestMetadata, IngressResponseMetadata,
    WorkloadEnvelopeParts, WorkloadPayload, WorkloadStreamKind, accept_workload_payload,
    finish_http_body, read_http_body_chunk, read_workload_frame, seal_workload_payload,
    write_http_body_chunk, write_workload_frame,
};
use reqwest::{
    Client, Method,
    header::{HeaderName, HeaderValue},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

use super::connection::ProxySession;
use crate::{SidecarConfig, SidecarEvent, egress_proxy::TunnelRequest};

const MAX_EGRESS_INITIAL_DATA_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct StreamMetrics {
    metrics: podmesh_metrics::Metrics,
    ingress: Arc<AtomicUsize>,
    egress: Arc<AtomicUsize>,
    websockets: Arc<AtomicUsize>,
}

impl StreamMetrics {
    pub fn new(metrics: podmesh_metrics::Metrics) -> Self {
        for gauge in [
            podmesh_metrics::GaugeName::ActiveIngress,
            podmesh_metrics::GaugeName::ActiveEgress,
            podmesh_metrics::GaugeName::ActiveWebsockets,
        ] {
            metrics.set_gauge(gauge, 0);
        }
        Self {
            metrics,
            ingress: Arc::new(AtomicUsize::new(0)),
            egress: Arc::new(AtomicUsize::new(0)),
            websockets: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn enter(&self, gauge: podmesh_metrics::GaugeName) -> ActiveGauge {
        let active = match gauge {
            podmesh_metrics::GaugeName::ActiveIngress => self.ingress.clone(),
            podmesh_metrics::GaugeName::ActiveEgress => self.egress.clone(),
            podmesh_metrics::GaugeName::ActiveWebsockets => self.websockets.clone(),
            _ => unreachable!("stream metrics accepts only active stream gauges"),
        };
        let current = active.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        self.metrics.set_gauge(gauge, current as u64);
        ActiveGauge {
            metrics: self.metrics.clone(),
            active,
            gauge,
        }
    }
}

struct ActiveGauge {
    metrics: podmesh_metrics::Metrics,
    active: Arc<AtomicUsize>,
    gauge: podmesh_metrics::GaugeName,
}

impl Drop for ActiveGauge {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        self.metrics
            .set_gauge(self.gauge, previous.saturating_sub(1) as u64);
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_connection(
    connection: Connection,
    connection_slots: Arc<Semaphore>,
    identity: iroh_support::NodeIdentity,
    replay_registry: Arc<protocol::PeerReplayRegistry>,
    proxy_grant: Vec<u8>,
    stream_metrics: StreamMetrics,
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
                    let connection_slots = connection_slots.clone();
                    let identity = identity.clone();
                    let replay_registry = replay_registry.clone();
                    let proxy_grant = proxy_grant.clone();
                    let stream_metrics = stream_metrics.clone();
                    tokio::spawn(async move {
                        let Ok(_connection_permit) = connection_slots.try_acquire_owned() else {
                            stream_metrics
                                .metrics
                                .record_event(podmesh_metrics::EventName::StreamSaturation);
                            return;
                        };
                        let permit = tokio::time::timeout(
                            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                            slots.acquire_owned(),
                        ).await;
                        let Ok(Ok(_permit)) = permit else {
                            stream_metrics
                                .metrics
                                .record_event(podmesh_metrics::EventName::StreamSaturation);
                            return;
                        };
                        let _active = stream_metrics
                            .enter(podmesh_metrics::GaugeName::ActiveIngress);
                        let timer = stream_metrics
                            .metrics
                            .operation_started(podmesh_metrics::OperationName::Ingress);
                        let result = handle_incoming(
                            remote,
                            send,
                            recv,
                            &identity,
                            &replay_registry,
                            &proxy_grant,
                            &config,
                            client,
                            &cancellation,
                            &stream_metrics,
                        ).await;
                        super::finish_operation(timer, &result, &stream_metrics.metrics);
                        if let Err(error) = result {
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

#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    remote: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
    identity: &iroh_support::NodeIdentity,
    replay_registry: &protocol::PeerReplayRegistry,
    proxy_grant: &[u8],
    config: &SidecarConfig,
    http_client: Client,
    cancellation: &CancellationToken,
    stream_metrics: &StreamMetrics,
) -> Result<()> {
    let (kind, payload) =
        read_workload_frame(&mut recv, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    ensure!(
        kind == WorkloadStreamKind::Ingress,
        "sidecar accepts only ingress streams"
    );
    let accepted = accept_workload_payload::<IngressRequestMetadata>(
        &payload,
        &identity.endpoint_id().to_string(),
        &remote.to_string(),
        now_millis()?,
        replay_registry,
        Instant::now(),
    )?;
    let mut request = accepted.payload;
    if request.target_port == 0 {
        request.target_port = config.app_port;
    }
    let upgrade_requested = request.upgrade_requested;
    let request_state = Arc::new(Mutex::new(Some((
        recv,
        protocol::HttpBodyProgress::default(),
        false,
    ))));
    let cancellation_for_body = cancellation.clone();
    let body_state = request_state.clone();
    let request_body = futures::stream::unfold(body_state, move |state| {
        let cancellation = cancellation_for_body.clone();
        async move {
            let (mut recv, mut progress, terminated) = state.lock().await.take()?;
            if terminated {
                *state.lock().await = Some((recv, progress, true));
                return None;
            }
            let result = read_http_body_chunk(
                &mut recv,
                &mut progress,
                DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                &cancellation,
            )
            .await;
            match result {
                Ok(Some(chunk)) => {
                    *state.lock().await = Some((recv, progress, false));
                    Some((Ok::<Vec<u8>, anyhow::Error>(chunk), state))
                }
                Ok(None) => {
                    *state.lock().await = Some((recv, progress, true));
                    None
                }
                Err(error) => Some((Err(error), state)),
            }
        }
    });
    let response = execute_local_http_request(
        http_client,
        request,
        reqwest::Body::wrap_stream(request_body),
    )
    .await?;
    let upgrade_accepted = upgrade_requested && response.status().as_u16() == 101;
    let metadata = IngressResponseMetadata {
        status_code: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect(),
        upgrade_accepted,
    };
    let mut response = Some(response);
    let upgraded = if upgrade_accepted {
        Some(
            response
                .take()
                .expect("response is present before upgrade")
                .upgrade()
                .await
                .context("complete local application HTTP upgrade")?,
        )
    } else {
        None
    };
    let payload = seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce: &crypto::generate_secure_nonce(),
            now_millis: now_millis()?,
            sender_id: &identity.endpoint_id().to_string(),
            recipient_id: &remote.to_string(),
            sender_signing_public: identity.signing_public(),
            sender_signing_private: identity.signing_private(),
            sender_kem_public: Some(identity.kem_public()),
        },
        &metadata,
    )?;
    write_workload_frame(
        &mut send,
        WorkloadStreamKind::Ingress,
        &payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        cancellation,
    )
    .await?;
    if let Some(upgraded) = upgraded {
        let websocket_timer = stream_metrics
            .metrics
            .operation_started(podmesh_metrics::OperationName::WebsocketRelay);
        let _active = stream_metrics.enter(podmesh_metrics::GaugeName::ActiveWebsockets);
        let (mut recv, mut request_progress, terminated) = request_state
            .lock()
            .await
            .take()
            .context("ingress request stream is unavailable after upgrade")?;
        if !terminated {
            ensure!(
                read_http_body_chunk(
                    &mut recv,
                    &mut request_progress,
                    DEFAULT_WORKLOAD_STREAM_TIMEOUT,
                    cancellation,
                )
                .await?
                .is_none(),
                "WebSocket upgrade request body was not consumed"
            );
        }
        let owner = config
            .owner_public_key_b64
            .as_deref()
            .context("sidecar owner key is missing")?;
        let owner_public = crypto::b64_decode(owner).context("decode sidecar owner key")?;
        let (application_read, application_write) = tokio::io::split(upgraded);
        let result = iroh_support::raw_relay::supervise_raw_relay(
            application_read,
            application_write,
            recv,
            send,
            websocket_relay_limits(),
            cancellation.clone(),
            || {
                protocol::verify_proxy_grant(
                    proxy_grant,
                    &owner_public,
                    owner,
                    &remote.to_string(),
                    super::now_secs(),
                )
            },
        )
        .await;
        super::finish_operation(websocket_timer, &result, &stream_metrics.metrics);
        result?;
        return Ok(());
    }
    let mut body_progress = protocol::HttpBodyProgress::default();
    let mut response_body = response
        .expect("non-upgrade response remains available")
        .bytes_stream();
    while let Some(chunk) = response_body.next().await {
        write_http_body_chunk(
            &mut send,
            &chunk.context("read local application response body")?,
            &mut body_progress,
            DEFAULT_WORKLOAD_STREAM_TIMEOUT,
            cancellation,
        )
        .await?;
    }
    finish_http_body(&mut send, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    send.finish().context("finish ingress response")?;
    Ok(())
}

fn websocket_relay_limits() -> iroh_support::raw_relay::RawRelayLimits {
    iroh_support::raw_relay::RawRelayLimits {
        max_bytes_per_direction: 512 * 1024 * 1024,
        idle_timeout: std::time::Duration::from_secs(300),
        max_lifetime: std::time::Duration::from_secs(60 * 60),
        authority_interval: std::time::Duration::from_secs(30),
    }
}

async fn execute_local_http_request(
    client: Client,
    request: IngressRequestMetadata,
    body: reqwest::Body,
) -> Result<reqwest::Response> {
    let method = Method::from_bytes(request.method.as_bytes()).context("invalid HTTP method")?;
    let url = format!(
        "http://127.0.0.1:{}{}",
        request.target_port, request.path_and_query
    );
    let mut builder = client.request(method, url);
    for (name, value) in request.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(body)
        .send()
        .await
        .context("send local HTTP request")
}

fn now_millis() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_millis()
        .try_into()
        .context("system time exceeds u64 milliseconds")
}

pub async fn open_egress(
    session: ProxySession,
    tunnel: TunnelRequest,
    cancellation: CancellationToken,
    stream_metrics: StreamMetrics,
    event_tx: Option<mpsc::UnboundedSender<SidecarEvent>>,
) -> Result<()> {
    let _active = stream_metrics.enter(podmesh_metrics::GaugeName::ActiveEgress);
    let timer = stream_metrics
        .metrics
        .operation_started(podmesh_metrics::OperationName::Egress);
    let destination_host = tunnel.dest_host.clone();
    let destination_port = tunnel.dest_port;
    let result = open_egress_inner(session, tunnel, &cancellation).await;
    super::finish_operation(timer, &result, &stream_metrics.metrics);
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
    session: ProxySession,
    mut tunnel: TunnelRequest,
    cancellation: &CancellationToken,
) -> Result<()> {
    let _connection_permit = session
        .stream_slots
        .clone()
        .try_acquire_owned()
        .context("proxy connection stream limit reached")?;
    let (mut send, mut recv) = tokio::time::timeout(
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        session.connection.open_bi(),
    )
    .await
    .context("egress stream open timed out")?
    .context("open egress stream")?;
    let request = EgressTunnelRequest::tcp(&tunnel.dest_host, tunnel.dest_port);
    let payload = seal_workload_payload(
        WorkloadEnvelopeParts {
            nonce: &crypto::generate_secure_nonce(),
            now_millis: now_millis()?,
            sender_id: &session.identity.endpoint_id().to_string(),
            recipient_id: &session.connection.remote_id().to_string(),
            sender_signing_public: session.identity.signing_public(),
            sender_signing_private: session.identity.signing_private(),
            sender_kem_public: Some(session.identity.kem_public()),
        },
        &request,
    )?;
    write_workload_frame(
        &mut send,
        <EgressTunnelRequest as WorkloadPayload>::TYPE.frame_kind(),
        &payload,
        DEFAULT_WORKLOAD_STREAM_TIMEOUT,
        cancellation,
    )
    .await?;
    let (kind, payload) =
        read_workload_frame(&mut recv, DEFAULT_WORKLOAD_STREAM_TIMEOUT, cancellation).await?;
    ensure!(
        kind == <EgressTunnelResponse as WorkloadPayload>::TYPE.frame_kind(),
        "unexpected egress response kind"
    );
    let response = accept_workload_payload::<EgressTunnelResponse>(
        &payload,
        &session.identity.endpoint_id().to_string(),
        &session.connection.remote_id().to_string(),
        now_millis()?,
        &session.replay_registry,
        Instant::now(),
    )?
    .payload;
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
    let initial_bytes = tunnel
        .initial_data
        .as_ref()
        .map_or(0, |initial_data| initial_data.len());
    if let Some(initial_data) = tunnel.initial_data.take() {
        ensure!(
            initial_data.len() <= MAX_EGRESS_INITIAL_DATA_BYTES,
            "egress initial data exceeds limit"
        );
        send.write_all(&initial_data).await?;
    }
    let (client_read, client_write) = tunnel.client_stream.into_split();
    let mut limits = iroh_support::raw_relay::RawRelayLimits {
        max_bytes_per_direction: 512 * 1024 * 1024,
        idle_timeout: std::time::Duration::from_secs(300),
        max_lifetime: std::time::Duration::from_secs(300),
        authority_interval: std::time::Duration::from_secs(30),
    };
    limits.max_bytes_per_direction = limits
        .max_bytes_per_direction
        .saturating_sub(u64::try_from(initial_bytes).context("convert initial egress bytes")?);
    iroh_support::raw_relay::supervise_raw_relay(
        client_read,
        client_write,
        recv,
        send,
        limits,
        cancellation.clone(),
        || {
            protocol::verify_proxy_grant(
                &session.proxy_grant,
                &session.owner_public,
                &session.owner_pubkey,
                &session.connection.remote_id().to_string(),
                super::now_secs(),
            )
        },
    )
    .await?;
    Ok(())
}
