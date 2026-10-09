use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result, ensure};
use podmesh_metrics::{
    ComponentName, MetricsConfig, MetricsRuntime, OPENMETRICS_CONTENT_TYPE, OperationName, Outcome,
    Reason,
};
use tokio_util::sync::CancellationToken;

fn loopback_ephemeral() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

#[tokio::test]
async fn every_runtime_exposes_its_own_bounded_openmetrics_document() -> Result<()> {
    let cases = [
        (ComponentName::Scheduler, OperationName::CapacityQuery),
        (ComponentName::Agent, OperationName::Admission),
        (ComponentName::Proxy, OperationName::Ingress),
        (ComponentName::Sidecar, OperationName::ProxyConnection),
    ];
    let client = reqwest::Client::new();

    for (component, operation) in cases {
        let runtime = MetricsRuntime::start(
            component,
            MetricsConfig {
                listen: Some(loopback_ephemeral()),
            },
            CancellationToken::new(),
        )
        .await?;
        runtime
            .metrics()
            .operation_started(operation)
            .finish(Outcome::Success, Reason::None);

        let response = client
            .get(format!("http://{}/metrics", runtime.local_addr().unwrap()))
            .send()
            .await
            .context("scrape runtime metrics")?;
        ensure!(response.status().is_success(), "metrics scrape failed");
        ensure!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                == Some(OPENMETRICS_CONTENT_TYPE),
            "metrics content type changed"
        );
        let body = response.text().await?;
        ensure!(body.len() <= podmesh_metrics::MAX_RENDERED_BYTES);
        ensure!(body.ends_with("# EOF\n"));
        ensure!(body.contains(&format!("component=\"{}\"", component.as_str())));
        ensure!(body.contains(operation.as_str()));
        runtime.shutdown().await?;
    }

    Ok(())
}

#[tokio::test]
async fn disabled_metrics_bind_no_socket_and_explicit_collision_fails() -> Result<()> {
    let disabled = MetricsRuntime::start(
        ComponentName::Scheduler,
        MetricsConfig::default(),
        CancellationToken::new(),
    )
    .await?;
    ensure!(disabled.local_addr().is_none());
    ensure!(!disabled.metrics().is_enabled());
    disabled.shutdown().await?;

    let occupied = tokio::net::TcpListener::bind(loopback_ephemeral()).await?;
    let result = MetricsRuntime::start(
        ComponentName::Proxy,
        MetricsConfig {
            listen: Some(occupied.local_addr()?),
        },
        CancellationToken::new(),
    )
    .await;
    ensure!(result.is_err(), "explicit bind collision must fail startup");
    Ok(())
}
