use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::{Endpoint, protocol::Router};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::{
    AgentControlHandler, AgentIdentity, SeenQueries, ValidatedMachineConfig,
    run_scheduler_attachment,
};
use crate::{AgentService, Config};

pub struct AgentMachine {
    endpoint: Endpoint,
    router: Router,
    cancellation: CancellationToken,
    supervisor: JoinHandle<Result<()>>,
}

impl AgentMachine {
    pub async fn start(config: &Config, service: AgentService) -> Result<Self> {
        // Scheduler URLs are resolved into signed EndpointRecords before
        // validation so both configuration styles converge on the same checks.
        let mut machine_config = config.machine.clone();
        if !machine_config.scheduler_urls.is_empty() {
            let bootstrapped =
                super::bootstrap::resolve_scheduler_urls(&machine_config.scheduler_urls).await?;
            machine_config.scheduler_endpoints.extend(bootstrapped);
        }
        let validated = machine_config.validate(now_secs())?;
        let identity = AgentIdentity::load(&config.key_dir)?;
        let endpoint = identity.bind(&validated).await?;
        let router = Router::builder(endpoint.clone())
            .accept(
                protocol::AGENT_CONTROL_ALPN,
                AgentControlHandler::new(service.clone(), validated.operation_timeout),
            )
            .spawn();
        let cancellation = CancellationToken::new();
        let seen = Arc::new(Mutex::new(SeenQueries::new(validated.max_seen_queries)));
        let supervisor = spawn_attachments(
            endpoint.clone(),
            validated,
            service,
            Arc::clone(&seen),
            cancellation.clone(),
        );
        log::info!("agent Iroh endpoint started: {}", identity.endpoint_id());
        Ok(Self {
            endpoint,
            router,
            cancellation,
            supervisor,
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub async fn join(&mut self) -> Result<Result<()>, tokio::task::JoinError> {
        (&mut self.supervisor).await
    }

    pub async fn shutdown(self) -> Result<()> {
        self.cancellation.cancel();
        self.router.shutdown().await?;
        self.endpoint.close().await;
        self.supervisor
            .await
            .context("join agent attachment supervisor")??;
        Ok(())
    }
}

fn spawn_attachments(
    endpoint: Endpoint,
    config: ValidatedMachineConfig,
    service: AgentService,
    seen: Arc<Mutex<SeenQueries>>,
    cancellation: CancellationToken,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        for scheduler in config.scheduler_endpoints.clone() {
            tasks.spawn(run_scheduler_attachment(
                endpoint.clone(),
                scheduler,
                config.clone(),
                service.clone(),
                Arc::clone(&seen),
                cancellation.clone(),
            ));
        }
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return Ok(());
                }
                result = tasks.join_next() => match result {
                    Some(Ok(Ok(()))) => {
                        anyhow::bail!("agent scheduler attachment stopped unexpectedly")
                    }
                    Some(Ok(Err(error))) => return Err(error),
                    Some(Err(error)) => return Err(error).context("agent attachment task failed"),
                    None => anyhow::bail!("agent has no scheduler attachment tasks"),
                }
            }
        }
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
