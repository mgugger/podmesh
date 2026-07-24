pub mod config;
pub mod runtime;
pub mod service;
pub mod sidecar;
pub mod store;

pub use config::Config;
pub use service::AgentService;

pub async fn run(config: Config) -> anyhow::Result<()> {
    let runtime = runtime::create_runtime(config.runtime, &config.workload_network)?;
    let service = AgentService::new(config.clone(), runtime).await?;
    let registration = service.spawn_registration_loop();
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    log::info!("podmesh agent listening on {}", listener.local_addr()?);
    let result = axum::serve(listener, service.router())
        .with_graceful_shutdown(shutdown())
        .await;
    registration.abort();
    result.map_err(Into::into)
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
