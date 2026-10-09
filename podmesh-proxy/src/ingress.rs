use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderValue, Request, Response, StatusCode},
    routing::any,
};
use axum_support::{parse_socket_addr, spawn_tcp_listener};
use log::{error, info, warn};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::iroh_runtime::ProxyClient;
use protocol::IngressRequestMetadata;
use protocol::MESH_DOMAIN_SUFFIX;

pub struct IngressServer {
    join: JoinHandle<()>,
    listen_addr: SocketAddr,
}

impl IngressServer {
    pub fn spawn(
        host: String,
        port: u16,
        sidecar: SidecarClient,
        routes: Option<Arc<crate::routes::RouteTable>>,
    ) -> Result<Self> {
        let addr = parse_socket_addr(&host, port)?;
        let std_listener = StdTcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;

        let state = IngressState { sidecar, routes };
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
    routes: Option<Arc<crate::routes::RouteTable>>,
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

    let Some(app_id) = state.resolve(&host) else {
        warn!("no workload claims host={host}");
        return status_response(StatusCode::NOT_FOUND, "no workload serves this host");
    };

    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    info!(
        "ingress proxy forwarding request via sidecar host={} manifest={} method={} path={}",
        host, app_id, method, path
    );

    match state.sidecar.forward(&app_id, request).await {
        Ok(response) => {
            let status = response.status().as_u16();
            info!(
                "ingress proxy received response from sidecar host={} manifest={} method={} path={} status={}",
                host, app_id, method, path, status
            );
            response
        }
        Err(err) => {
            error!(
                "sidecar forward failed host={} manifest={} error={}",
                host, app_id, err
            );
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

impl IngressState {
    /// Map a request hostname to the routing key that serves it.
    ///
    /// Only two things resolve: a hostname a workload actually claimed, and the
    /// canonical `<routing-key>.mesh.local` form. Treating an unrecognised Host
    /// header as a routing key — as an earlier version did — would let any
    /// client address any workload just by naming it.
    fn resolve(&self, host: &str) -> Option<String> {
        let routes = self.routes.as_ref()?;
        if let Some(manifest_id) = routes.resolve_host(host) {
            return Some(manifest_id);
        }
        let canonical = canonical_routing_key(host)?;
        routes.contains(&canonical).then_some(canonical)
    }
}

/// Extract the routing key from `<routing-key>.mesh.local`.
///
/// The key is a 32-byte blake3 digest in hex, so anything else is rejected
/// outright rather than passed through as a routing key.
fn canonical_routing_key(host: &str) -> Option<String> {
    let suffix = format!(".{MESH_DOMAIN_SUFFIX}");
    let label = host.strip_suffix(&suffix)?.rsplit('.').next()?;
    (label.len() == 64 && label.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| label.to_ascii_lowercase())
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
    Arc::new(NoopSidecarForwarder)
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
        mut request: Request<Body>,
    ) -> Result<Response<Body>, SidecarError> {
        let upgrade_requested = request
            .headers()
            .get(axum::http::header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
            && request.headers().contains_key(axum::http::header::UPGRADE);
        let on_upgrade = upgrade_requested.then(|| hyper::upgrade::on(&mut request));
        let (parts, body) = request.into_parts();
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
        let proxy_request = IngressRequestMetadata {
            manifest_id: app_id.to_string(),
            method: parts.method.to_string(),
            path_and_query,
            headers,
            target_port: 0,
            upgrade_requested,
        };
        self.proxy
            .forward(proxy_request, body, on_upgrade)
            .await
            .map_err(|err| SidecarError::ForwardFailed(err.to_string()))
    }
}
