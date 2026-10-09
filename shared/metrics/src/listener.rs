#![forbid(unsafe_code)]

use std::{convert::Infallible, fmt, net::SocketAddr, sync::Arc, time::Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body as _, Incoming},
    header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{JoinHandle, JoinSet},
    time::{Instant as TokioInstant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{
    Metrics, OperationName, Outcome, Reason,
    config::{
        CONNECTION_TIMEOUT, MAX_CONCURRENT_REQUESTS, MAX_CONNECTIONS, MAX_REQUEST_BYTES,
        SHUTDOWN_DRAIN_TIMEOUT,
    },
    diagnostics::{DiagnosticAction, DiagnosticSuppressor, FailureCategory},
    encoding::{OPENMETRICS_CONTENT_TYPE, render_snapshot},
};

const HEALTH_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

#[derive(Debug)]
pub enum ListenerError {
    Bind(std::io::Error),
    Join(tokio::task::JoinError),
}

impl fmt::Display for ListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bind(_) => "metrics listener bind failed",
            Self::Join(_) => "metrics listener task failed",
        })
    }
}

impl std::error::Error for ListenerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            Self::Join(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub struct MetricsListener {
    local_addr: SocketAddr,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl MetricsListener {
    pub async fn start(
        listen: SocketAddr,
        metrics: Metrics,
        parent_cancellation: CancellationToken,
    ) -> Result<Self, ListenerError> {
        let listener = TcpListener::bind(listen)
            .await
            .map_err(ListenerError::Bind)?;
        let local_addr = listener.local_addr().map_err(ListenerError::Bind)?;
        let cancellation = parent_cancellation.child_token();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            run_listener(listener, metrics, task_cancellation).await;
        });
        Ok(Self {
            local_addr,
            cancellation,
            task: Some(task),
        })
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) -> Result<(), ListenerError> {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(ListenerError::Join)?;
        }
        Ok(())
    }
}

impl Drop for MetricsListener {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

async fn run_listener(listener: TcpListener, metrics: Metrics, cancellation: CancellationToken) {
    let connection_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let request_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let diagnostics = Arc::new(Mutex::new(DiagnosticSuppressor::default()));
    let mut connections = JoinSet::new();

    loop {
        let permit = tokio::select! {
            _ = cancellation.cancelled() => break,
            permit = connection_slots.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            _ = cancellation.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, _) = match accepted {
            Ok(accepted) => accepted,
            Err(_) => {
                log_listener_failure(&diagnostics, FailureCategory::Listener);
                continue;
            }
        };
        let task_metrics = metrics.clone();
        let task_slots = request_slots.clone();
        let task_cancellation = cancellation.clone();
        let task_diagnostics = diagnostics.clone();
        connections.spawn(async move {
            serve_connection(
                stream,
                permit,
                task_metrics,
                task_slots,
                task_cancellation,
                task_diagnostics,
            )
            .await;
        });
    }

    let drain = async { while connections.join_next().await.is_some() {} };
    if timeout(SHUTDOWN_DRAIN_TIMEOUT, drain).await.is_err() {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}

async fn serve_connection(
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
    metrics: Metrics,
    request_slots: Arc<Semaphore>,
    cancellation: CancellationToken,
    diagnostics: Arc<Mutex<DiagnosticSuppressor>>,
) {
    let service =
        service_fn(move |request| handle_request(request, metrics.clone(), request_slots.clone()));
    let mut builder = http1::Builder::new();
    builder
        .keep_alive(false)
        .half_close(false)
        .max_buf_size(MAX_REQUEST_BYTES);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    let deadline = TokioInstant::now() + CONNECTION_TIMEOUT;
    tokio::select! {
        result = timeout_at(deadline, &mut connection) => {
            if !matches!(result, Ok(Ok(()))) {
                log_listener_failure(&diagnostics, FailureCategory::Listener);
            }
        }
        _ = cancellation.cancelled() => {
            connection.as_mut().graceful_shutdown();
            if timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut connection).await.is_err() {
                log_listener_failure(&diagnostics, FailureCategory::Listener);
            }
        }
    }
}

async fn handle_request(
    request: Request<Incoming>,
    metrics: Metrics,
    request_slots: Arc<Semaphore>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request_has_body(&request) {
        return Ok(empty_response(StatusCode::PAYLOAD_TOO_LARGE));
    }
    let Ok(_permit) = request_slots.try_acquire_owned() else {
        return Ok(empty_response(StatusCode::SERVICE_UNAVAILABLE));
    };

    let method = request.method().clone();
    let path = request.uri().path();
    if method != Method::GET {
        return Ok(empty_response(StatusCode::METHOD_NOT_ALLOWED));
    }
    if path == "/health" {
        return Ok(response(
            StatusCode::OK,
            HEALTH_CONTENT_TYPE,
            Bytes::from_static(b"ok\n"),
        ));
    }
    if path != "/metrics" {
        return Ok(empty_response(StatusCode::NOT_FOUND));
    }

    let timer = metrics.operation_started(OperationName::MetricsScrape);
    let Some(snapshot) = metrics.snapshot() else {
        timer.finish(Outcome::Error, Reason::Internal);
        return Ok(empty_response(StatusCode::SERVICE_UNAVAILABLE));
    };
    match render_snapshot(&snapshot, Instant::now() + CONNECTION_TIMEOUT) {
        Ok(document) => {
            timer.finish(Outcome::Success, Reason::None);
            Ok(response(
                StatusCode::OK,
                OPENMETRICS_CONTENT_TYPE,
                Bytes::from(document.into_string()),
            ))
        }
        Err(crate::RenderError::Deadline) => {
            timer.finish(Outcome::Timeout, Reason::Deadline);
            Ok(empty_response(StatusCode::SERVICE_UNAVAILABLE))
        }
        Err(_) => {
            timer.finish(Outcome::Error, Reason::Internal);
            Ok(empty_response(StatusCode::SERVICE_UNAVAILABLE))
        }
    }
}

fn request_has_body(request: &Request<Incoming>) -> bool {
    request.headers().contains_key(TRANSFER_ENCODING)
        || request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value != "0")
        || !request.body().is_end_stream()
}

fn empty_response(status: StatusCode) -> Response<Full<Bytes>> {
    response(status, HEALTH_CONTENT_TYPE, Bytes::new())
}

fn response(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static(content_type),
    );
    response
        .headers_mut()
        .insert(CONNECTION, hyper::header::HeaderValue::from_static("close"));
    response
}

fn log_listener_failure(diagnostics: &Mutex<DiagnosticSuppressor>, category: FailureCategory) {
    match diagnostics.lock().observe(category, Instant::now()) {
        DiagnosticAction::Silent => {}
        DiagnosticAction::LogFirst { category } => {
            log::warn!("metrics listener failure category={}", category.as_str());
        }
        DiagnosticAction::LogSummary {
            category,
            suppressed_count,
        } => log::warn!(
            "metrics listener failures category={} suppressed_count={}",
            category.as_str(),
            suppressed_count
        ),
    }
}
