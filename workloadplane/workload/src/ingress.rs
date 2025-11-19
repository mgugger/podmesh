use std::collections::HashMap;
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
use parking_lot::RwLock;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::p2p::ProxyClient;
use p2p::http_proxy::ProxyHttpRequest;

const DEFAULT_DOMAIN_SUFFIX: &str = "mesh.com";
const MAX_PROXY_BODY_BYTES: usize = 4 * 1024 * 1024;

pub struct IngressServer {
    join: JoinHandle<()>,
    routes: IngressRoutes,
    listen_addr: SocketAddr,
}

impl IngressServer {
    pub fn spawn(host: String, port: u16, gateway: GatewayClient) -> Result<Self> {
        let addr = parse_socket_addr(&host, port)?;
        let std_listener = StdTcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;

        let routes = IngressRoutes::default();
        let state = IngressState {
            routes: routes.clone(),
            gateway,
        };

        let app = Router::new().fallback(any(ingress_entry)).with_state(state);
        let join = spawn_tcp_listener(listener, app, "workload-ingress");

        info!(addr = %addr, "ingress server listening");
        Ok(Self {
            join,
            routes,
            listen_addr: addr,
        })
    }

    pub fn routes(&self) -> IngressRoutes {
        self.routes.clone()
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub async fn shutdown(self) {
        self.join.abort();
        let _ = self.join.await;
    }
}

#[derive(Clone, Default)]
pub struct IngressRoutes {
    inner: Arc<RwLock<HashMap<String, Vec<RouteMapping>>>>,
}

impl IngressRoutes {
    pub fn register(&self, app_id: impl Into<String>, spec: IngressRouteSpec) {
        let mut map = self.inner.write();
        let entry = map.entry(app_id.into()).or_default();
        entry.push(RouteMapping::from(spec));
        entry.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));
    }

    pub fn resolve(&self, app_id: &str, path: &str) -> Option<RouteMapping> {
        let map = self.inner.read();
        let routes = map.get(app_id)?;
        routes.iter().find(|route| route.matches(path)).cloned()
    }
}

#[derive(Clone, Debug)]
pub struct IngressRouteSpec {
    pub path_prefix: String,
    pub target_port: u16,
}

#[derive(Clone, Debug)]
pub struct RouteMapping {
    pub path_prefix: String,
    pub target_port: u16,
}

impl RouteMapping {
    fn matches(&self, path: &str) -> bool {
        path.starts_with(&self.path_prefix)
    }
}

impl From<IngressRouteSpec> for RouteMapping {
    fn from(value: IngressRouteSpec) -> Self {
        let mut prefix = value.path_prefix;
        if prefix.is_empty() {
            prefix = "/".to_string();
        } else if !prefix.starts_with('/') {
            prefix = format!("/{}", prefix);
        }
        Self {
            path_prefix: prefix,
            target_port: value.target_port,
        }
    }
}

#[derive(Clone)]
pub struct IngressState {
    routes: IngressRoutes,
    gateway: GatewayClient,
}

async fn ingress_entry(
    State(state): State<IngressState>,
    request: Request<Body>,
) -> Response<Body> {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| parse_host(value));

    let Some(app_id) = host else {
        return status_response(StatusCode::BAD_REQUEST, "missing host header");
    };

    let path = request.uri().path().to_string();
    let Some(route) = state.routes.resolve(&app_id, &path) else {
        return status_response(StatusCode::NOT_FOUND, "no ingress route matched");
    };

    match state.gateway.forward(&app_id, route.clone(), request).await {
        Ok(response) => response,
        Err(err) => {
            warn!(app_id = %app_id, error = %err, "gateway forward failed");
            status_response(StatusCode::BAD_GATEWAY, "gateway forwarding failed")
        }
    }
}

fn parse_host(value: &HeaderValue) -> Option<String> {
    let raw = value.to_str().ok()?;
    let host_part = raw.split(':').next()?.trim();
    if host_part.is_empty() {
        return None;
    }
    let suffix = format!(".{}", DEFAULT_DOMAIN_SUFFIX);
    if let Some(stripped) = host_part.strip_suffix(&suffix) {
        if !stripped.is_empty() {
            return Some(stripped.to_string());
        }
    } else {
        debug!(
            host = host_part,
            expected_suffix = DEFAULT_DOMAIN_SUFFIX,
            "ingress host missing expected suffix"
        );
        return host_part.split('.').next().map(|s| s.to_string());
    }
    None
}

fn status_response(code: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("invalid response")))
}

pub type GatewayClient = Arc<dyn GatewayForwarder + Send + Sync>;

pub fn proxy_gateway_client(proxy_client: ProxyClient) -> GatewayClient {
    Arc::new(ProxyGatewayForwarder::new(proxy_client))
}

#[async_trait]
pub trait GatewayForwarder {
    async fn forward(
        &self,
        app_id: &str,
        route: RouteMapping,
        request: Request<Body>,
    ) -> Result<Response<Body>, GatewayError>;
}

#[derive(thiserror::Error, Debug)]
pub enum GatewayError {
    #[error("no gateway registered for app {0}")]
    MissingGateway(String),
    #[error("forwarding failed: {0}")]
    ForwardFailed(String),
}

#[derive(Clone, Default)]
pub struct NoopGatewayForwarder;

#[async_trait]
impl GatewayForwarder for NoopGatewayForwarder {
    async fn forward(
        &self,
        app_id: &str,
        route: RouteMapping,
        _request: Request<Body>,
    ) -> Result<Response<Body>, GatewayError> {
        let body = format!(
            "gateway forwarding not implemented (app={}, port={}, path={})",
            app_id, route.target_port, route.path_prefix
        );
        Ok(Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .body(Body::from(body))
            .unwrap())
    }
}

pub fn noop_gateway_client() -> GatewayClient {
    Arc::new(NoopGatewayForwarder::default())
}

#[derive(Clone)]
struct ProxyGatewayForwarder {
    proxy: ProxyClient,
}

impl ProxyGatewayForwarder {
    fn new(proxy: ProxyClient) -> Self {
        Self { proxy }
    }
}

#[async_trait]
impl GatewayForwarder for ProxyGatewayForwarder {
    async fn forward(
        &self,
        app_id: &str,
        route: RouteMapping,
        request: Request<Body>,
    ) -> Result<Response<Body>, GatewayError> {
        let (parts, body) = request.into_parts();
        let body_bytes = to_bytes(body, MAX_PROXY_BODY_BYTES)
            .await
            .map_err(|err| GatewayError::ForwardFailed(format!("read body failed: {err}")))?;
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
            target_port: route.target_port,
        };
        let proxy_response = self
            .proxy
            .forward(proxy_request)
            .await
            .map_err(|err| GatewayError::ForwardFailed(err.to_string()))?;
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
            .map_err(|err| GatewayError::ForwardFailed(format!("build response failed: {err}")))
    }
}
