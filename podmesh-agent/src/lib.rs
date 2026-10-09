pub mod config;
pub mod machine;
pub mod runtime;
pub mod service;
pub mod sidecar;
pub mod store;

pub use config::Config;
pub use service::AgentService;

use std::{future::IntoFuture, time::Duration};

use anyhow::Context;
use tokio_util::sync::CancellationToken;

const SERVICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(config: Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    if let Some(metrics_listen) = config.metrics_listen {
        anyhow::ensure!(
            !podmesh_metrics::listeners_conflict(metrics_listen, listener.local_addr()?)
                && !podmesh_metrics::listeners_conflict(metrics_listen, config.machine.bind_addr),
            "agent metrics listener conflicts with an existing listener"
        );
    }
    let cancellation = CancellationToken::new();
    let metrics_runtime = podmesh_metrics::MetricsRuntime::start(
        podmesh_metrics::ComponentName::Agent,
        podmesh_metrics::MetricsConfig {
            listen: config.metrics_listen,
        },
        cancellation.clone(),
    )
    .await?;
    let runtime = runtime::create_runtime(config.runtime, &config.workload_network)?;
    let service =
        AgentService::new_with_metrics(config.clone(), runtime, metrics_runtime.metrics()).await?;
    let mut machine = machine::AgentMachine::start(&config, service.clone()).await?;
    log::info!("podmesh agent listening on {}", listener.local_addr()?);

    let http_cancellation = cancellation.clone();
    let http_server = axum::serve(listener, service.router())
        .with_graceful_shutdown(async move { http_cancellation.cancelled().await })
        .into_future();
    tokio::pin!(http_server);

    tokio::select! {
        signal_result = shutdown_signal() => {
            signal_result?;
            cancellation.cancel();
            shutdown_http(&mut http_server).await?;
            shutdown_machine(machine).await?;
            metrics_runtime.shutdown().await?;
            Ok(())
        }
        http_result = &mut http_server => {
            cancellation.cancel();
            shutdown_machine(machine).await?;
            metrics_runtime.shutdown().await?;
            http_result?;
            anyhow::bail!("agent HTTP service stopped unexpectedly")
        }
        machine_result = machine.join() => {
            cancellation.cancel();
            shutdown_http(&mut http_server).await?;
            shutdown_machine(machine).await?;
            metrics_runtime.shutdown().await?;
            machine_result.context("agent machine supervisor task failed")??;
            anyhow::bail!("agent machine plane stopped unexpectedly")
        }
    }
}

async fn shutdown_http<F>(http_server: &mut std::pin::Pin<&mut F>) -> anyhow::Result<()>
where
    F: Future<Output = std::io::Result<()>>,
{
    tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, http_server)
        .await
        .context("agent HTTP graceful shutdown timed out")??;
    Ok(())
}

async fn shutdown_machine(machine: machine::AgentMachine) -> anyhow::Result<()> {
    tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, machine.shutdown())
        .await
        .context("agent machine shutdown timed out")??;
    Ok(())
}

async fn shutdown_signal() -> anyhow::Result<()> {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| anyhow::anyhow!("install Ctrl-C handler: {error}"))
    };
    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|error| anyhow::anyhow!("install SIGTERM handler: {error}"))?;
        signal.recv().await;
        anyhow::Result::<()>::Ok(())
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<anyhow::Result<()>>();
    tokio::select! { result = ctrl_c => result, result = terminate => result }
}
