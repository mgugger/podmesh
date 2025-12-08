use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderName, HeaderValue, Request, Response, StatusCode},
    routing::any,
};
use axum_support::{parse_socket_addr, spawn_tcp_listener};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use log::{error, info, warn};

use crate::p2p::ProxyClient;
use p2p::http_proxy::ProxyHttpRequest;
use protocol::libp2p_constants::MESH_DOMAIN_SUFFIX;
const MAX_PROXY_BODY_BYTES: usize = 4 * 1024 * 1024;

pub struct IngressServer {
    join: JoinHandle<()>,
    listen_addr: SocketAddr,
}

impl IngressServer {
    pub fn spawn(host: String, port: u16, sidecar: SidecarClient) -> Result<Self> {
        let addr = parse_socket_addr(&host, port)?;
        let std_listener = StdTcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;

        let state = IngressState { sidecar };
        let app = Router::new().fallback(any(ingress_entry)).with_state(state);
        let join = spawn_tcp_listener(listener, app, "workload-ingress");

        info!("ingress server listening addr={}", addr);
        Ok(Self {
            join,
            listen_addr: addr,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub async fn shutdown(self) {
        self.join.abort();
        let _ = self.join.await;
    }
}

#[derive(Clone)]
pub struct IngressState {
    sidecar: SidecarClient,
}

async fn ingress_entry(
    State(state): State<IngressState>,
    request: Request<Body>,
) -> Response<Body> {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(parse_host);

    let Some(host) = host else {
        return status_response(StatusCode::BAD_REQUEST, "missing host header");
    };

    let Some(app_id) = manifest_id_from_host(&host) else {
        warn!("unable to derive manifest id from host host={}", host);
        return status_response(StatusCode::BAD_REQUEST, "invalid host header");
    };

    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    info!("ingress proxy forwarding request via sidecar host={} manifest={} method={} path={}", host, app_id, method, path);

    match state.sidecar.forward(&app_id, request).await {
        Ok(response) => {
            let status = response.status().as_u16();
            info!("ingress proxy received response from sidecar host={} manifest={} method={} path={} status={}", host, app_id, method, path, status);
            response
        }
        Err(err) => {
            error!("sidecar forward failed host={} manifest={} error={}", host, app_id, err);
            status_response(StatusCode::BAD_GATEWAY, "sidecar forwarding failed")
        }
    }
}

fn parse_host(value: &HeaderValue) -> Option<String> {
    let raw = value.to_str().ok()?;
    let host_part = raw.split(':').next()?.trim();
    if host_part.is_empty() {
        return None;
    }
    Some(host_part.trim_end_matches('.').to_lowercase())
}

fn manifest_id_from_host(host: &str) -> Option<String> {
    if host.is_empty() {
        return None;
    }
    let suffix = format!(".{}", MESH_DOMAIN_SUFFIX);
    if let Some(stripped) = host.strip_suffix(&suffix) {
        return stripped
            .trim_matches('.')
            .rsplit('.')
            .next()
            .filter(|segment| !segment.is_empty())
            .map(|segment| segment.to_string());
    }
    Some(host.to_string())
}

fn status_response(code: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("invalid response")))
}

pub type SidecarClient = Arc<dyn SidecarForwarder + Send + Sync>;

pub fn proxy_sidecar_client(proxy_client: ProxyClient) -> SidecarClient {
    Arc::new(ProxySidecarForwarder::new(proxy_client))
}

#[async_trait]
pub trait SidecarForwarder {
    async fn forward(
        &self,
        app_id: &str,
        request: Request<Body>,
    ) -> Result<Response<Body>, SidecarError>;
}

#[derive(thiserror::Error, Debug)]
pub enum SidecarError {
    #[error("no sidecar registered for app {0}")]
    MissingSidecar(String),
    #[error("forwarding failed: {0}")]
    ForwardFailed(String),
}

#[derive(Clone, Default)]
pub struct NoopSidecarForwarder;

#[async_trait]
impl SidecarForwarder for NoopSidecarForwarder {
    async fn forward(
        &self,
        app_id: &str,
        _request: Request<Body>,
    ) -> Result<Response<Body>, SidecarError> {
        let body = format!("sidecar forwarding not implemented (app={})", app_id);
        Ok(Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .body(Body::from(body))
            .unwrap())
    }
}

pub fn noop_sidecar_client() -> SidecarClient {
    Arc::new(NoopSidecarForwarder::default())
}

#[derive(Clone)]
struct ProxySidecarForwarder {
    proxy: ProxyClient,
}

impl ProxySidecarForwarder {
    fn new(proxy: ProxyClient) -> Self {
        Self { proxy }
    }
}

#[async_trait]
impl SidecarForwarder for ProxySidecarForwarder {
    async fn forward(
        &self,
        app_id: &str,
        request: Request<Body>,
    ) -> Result<Response<Body>, SidecarError> {
        let (parts, body) = request.into_parts();
        let body_bytes = to_bytes(body, MAX_PROXY_BODY_BYTES)
            .await
            .map_err(|err| SidecarError::ForwardFailed(format!("read body failed: {err}")))?;
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| parts.uri.path().to_string());
        let headers = parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                let header_value = value.to_str().ok()?.to_string();
                Some((name.as_str().to_string(), header_value))
            })
            .collect();
        let proxy_request = ProxyHttpRequest {
            manifest_id: app_id.to_string(),
            method: parts.method.to_string(),
            path_and_query,
            headers,
            body: body_bytes.to_vec(),
            target_port: 0,
        };
        let proxy_response = self
            .proxy
            .forward(proxy_request)
            .await
            .map_err(|err| SidecarError::ForwardFailed(err.to_string()))?;
        let status =
            StatusCode::from_u16(proxy_response.status_code).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut builder = Response::builder().status(status);
        for (name, value) in proxy_response.headers {
            if let (Ok(header_name), Ok(header_value)) =
                (HeaderName::from_str(&name), HeaderValue::from_str(&value))
            {
                builder = builder.header(header_name, header_value);
            }
        }
        builder
            .body(Body::from(proxy_response.body))
            .map_err(|err| SidecarError::ForwardFailed(format!("build response failed: {err}")))
    }
}
