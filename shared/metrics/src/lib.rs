pub mod catalog;
pub mod config;
pub mod diagnostics;
pub mod encoding;
pub mod listener;
pub mod recorder;
pub mod registry;
pub mod snapshot;
pub mod startup;
pub mod timer;

pub use catalog::{
    ComponentName, EventName, GaugeName, OperationGroup, OperationName, Outcome, Reason,
};
pub use config::{
    CONNECTION_TIMEOUT, MAX_CONCURRENT_REQUESTS, MAX_CONNECTIONS, MAX_REQUEST_BYTES, MetricsConfig,
    SHUTDOWN_DRAIN_TIMEOUT, listeners_conflict,
};
pub use encoding::{
    MAX_RENDERED_BYTES, OPENMETRICS_CONTENT_TYPE, RenderError, RenderedMetricsDocument,
    render_snapshot,
};
pub use recorder::Metrics;
pub use snapshot::{HistogramValue, MetricsSnapshot};
pub use startup::MetricsRuntime;
pub use timer::OperationTimer;

#[cfg(test)]
mod properties;

#[cfg(test)]
mod listener_tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        sync::Arc,
        time::Duration,
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Semaphore;
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn request(addr: SocketAddr, raw: &[u8]) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    async fn runtime(component: ComponentName) -> MetricsRuntime {
        MetricsRuntime::start(
            component,
            MetricsConfig {
                listen: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn disabled_runtime_binds_no_listener_and_uses_noop_metrics() {
        let runtime = MetricsRuntime::start(
            ComponentName::Agent,
            MetricsConfig::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(runtime.local_addr(), None);
        assert!(!runtime.metrics().is_enabled());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn health_response_is_exact_and_listener_only() {
        let runtime = runtime(ComponentName::Scheduler).await;
        let response = request(
            runtime.local_addr().unwrap(),
            b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("content-type: text/plain; charset=utf-8\r\n"));
        assert!(response.ends_with("\r\nok\n"));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn metrics_response_is_openmetrics_and_component_scoped() {
        let runtime = runtime(ComponentName::Proxy).await;
        let response = request(
            runtime.local_addr().unwrap(),
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains(OPENMETRICS_CONTENT_TYPE));
        assert!(response.contains("component=\"proxy\""));
        assert!(response.ends_with("# EOF\n"));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn route_method_and_body_failures_are_fixed() {
        for (request_bytes, expected) in [
            (
                &b"GET /missing HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
                "404 Not Found",
            ),
            (
                &b"POST /metrics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"[..],
                "405 Method Not Allowed",
            ),
            (
                &b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx"[..],
                "413 Payload Too Large",
            ),
        ] {
            let runtime = runtime(ComponentName::Sidecar).await;
            let response = request(runtime.local_addr().unwrap(), request_bytes).await;
            assert!(response.starts_with(&format!("HTTP/1.1 {expected}\r\n")));
            runtime.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn explicit_bind_collision_fails_startup() {
        let occupied = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let result = MetricsRuntime::start(
            ComponentName::Agent,
            MetricsConfig {
                listen: Some(occupied.local_addr().unwrap()),
            },
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn admission_semaphores_refuse_one_past_each_exact_limit() {
        for limit in [MAX_CONNECTIONS, MAX_CONCURRENT_REQUESTS] {
            let slots = Arc::new(Semaphore::new(limit));
            let permits: Vec<_> = (0..limit)
                .map(|_| slots.clone().try_acquire_owned().unwrap())
                .collect();
            assert_eq!(slots.available_permits(), 0);
            assert!(slots.clone().try_acquire_owned().is_err());
            drop(permits);
            assert_eq!(slots.available_permits(), limit);
        }
    }

    #[tokio::test]
    async fn oversized_request_head_is_refused_before_service_dispatch() {
        let runtime = runtime(ComponentName::Agent).await;
        let mut request_bytes = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nX-Large: ".to_vec();
        request_bytes.extend(std::iter::repeat_n(b'a', MAX_REQUEST_BYTES));
        request_bytes.extend_from_slice(b"\r\n\r\n");
        let response = request(runtime.local_addr().unwrap(), &request_bytes).await;
        assert!(!response.starts_with("HTTP/1.1 200 OK\r\n"));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_an_idle_connection_within_the_drain_bound() {
        let runtime = runtime(ComponentName::Scheduler).await;
        let _idle = tokio::net::TcpStream::connect(runtime.local_addr().unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("idle connection should be cancelled without waiting for its read deadline")
            .unwrap();
    }
}
