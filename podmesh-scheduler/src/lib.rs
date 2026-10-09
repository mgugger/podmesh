use anyhow::Context;
use clap::Parser;
use std::{
    future::IntoFuture,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

pub mod clientapi;
pub mod machine;
pub mod relay;
pub use machine::MachineConfig;
pub use relay::MachineRelayConfig;

const SERVICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Sub-directory of the scheduler key directory holding the self-provisioned
/// machine relay certificate and private key.
const MACHINE_RELAY_CREDENTIAL_DIR: &str = "machine-relay";

/// Fills in the machine relay credentials that an operator did not supply.
///
/// A scheduler already owns a signing key and a private key directory, so
/// requiring an out-of-band TLS pair and a hand-copied trusted issuer list only
/// blocks the first start. A relay that trusts nobody is useless, and the one
/// issuer it must always trust is the scheduler it belongs to, so an empty
/// trust list defaults to exactly that. Operators running a multi-scheduler
/// relay still list every peer key explicitly and nothing is inferred.
fn provision_relay_credentials(
    config: &mut Config,
    identity: &machine::SchedulerIdentity,
) -> anyhow::Result<()> {
    if config.relay.trusted_issuer_keys.is_empty() {
        config.relay.trusted_issuer_keys = vec![crypto::b64_encode(identity.signing_public())];
        log::info!("machine relay trusts this scheduler's signing key by default");
    }
    if config.relay.certificate_mode == relay::CertificateMode::Manual
        && config.relay.tls_certificate.is_none()
        && config.relay.tls_private_key.is_none()
    {
        let material = iroh_support::ensure_relay_tls(
            &config.machine.key_dir.join(MACHINE_RELAY_CREDENTIAL_DIR),
            &config.relay.audience,
            None,
            None,
        )
        .context("provision machine relay TLS material")?;
        config.relay.tls_certificate = Some(material.certificate_path);
        config.relay.tls_private_key = Some(material.private_key_path);
    }
    Ok(())
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Config {
    #[arg(long, env = "PODMESH_SCHEDULER_LISTEN", default_value = "0.0.0.0:3000")]
    pub listen: String,

    #[arg(long = "metrics-listen", env = "PODMESH_METRICS_LISTEN")]
    pub application_metrics_listen: Option<std::net::SocketAddr>,

    /// Requests per minute a single peer address may make against the client
    /// API.
    ///
    /// The API is unauthenticated by design, so this is what keeps a single
    /// caller from enumerating the fleet or occupying placement capacity. Zero
    /// disables throttling, which is only appropriate where every caller shares
    /// one source address.
    #[arg(
        long = "client-rate-limit-per-minute",
        env = "PODMESH_SCHEDULER_CLIENT_RATE_LIMIT",
        default_value_t = clientapi::DEFAULT_CLIENT_RATE_LIMIT_PER_MINUTE
    )]
    pub client_rate_limit_per_minute: u32,

    #[command(flatten)]
    pub relay: MachineRelayConfig,

    #[command(flatten)]
    pub machine: MachineConfig,
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let mut config = config;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    let identity = machine::SchedulerIdentity::load(&config.machine.key_dir)?;
    provision_relay_credentials(&mut config, &identity)?;
    identity.validate_relay_trust(&config.relay)?;
    let machine_config = config.machine.validate(identity.endpoint_id())?;
    if let Some(metrics_listen) = config.application_metrics_listen {
        let application_listen = listener.local_addr()?;
        let listeners = [
            application_listen,
            machine_config.bind_addr,
            config.relay.http_listen,
            config.relay.https_listen,
            config.relay.qad_listen,
            config.relay.metrics_listen,
        ];
        anyhow::ensure!(
            listeners
                .into_iter()
                .all(|other| !podmesh_metrics::listeners_conflict(metrics_listen, other)),
            "scheduler metrics listener conflicts with an existing listener"
        );
    }
    let cancellation = CancellationToken::new();
    let metrics_runtime = podmesh_metrics::MetricsRuntime::start(
        podmesh_metrics::ComponentName::Scheduler,
        podmesh_metrics::MetricsConfig {
            listen: config.application_metrics_listen,
        },
        cancellation.clone(),
    )
    .await?;
    let metrics = metrics_runtime.metrics();
    let local_relay_audience = config.relay.canonical_audience()?;
    let configured_relay_urls =
        iroh::RelayMap::try_from_iter(machine_config.relay_urls.iter().map(String::as_str))?
            .urls::<Vec<_>>();
    anyhow::ensure!(
        configured_relay_urls
            .iter()
            .any(|relay_url| relay_url.to_string() == local_relay_audience),
        "integrated scheduler relay must be present in the machine relay map"
    );
    let relay_access = relay::MachineRelayAccessControl::from_config(&config.relay)?;
    let relay_issuers = relay_access.issuers();
    // Announced schedulers bind their signing key to their endpoint here, which
    // is what lets a scheduler behind NAT reach a peer's relay without being
    // pinned by that peer's operator.
    let member_issuers = relay_access.member_issuers();
    let peer_urls = config.machine.scheduler_peer_urls.clone();
    let configured_pins = config
        .machine
        .scheduler_peer_pins
        .iter()
        .map(|value| machine::parse_pin_argument(value))
        .collect::<anyhow::Result<Vec<_>>>()
        .context("parse configured scheduler peer pins")?;
    let mut relay_server = relay::start(config.relay, relay_access).await?;
    let machine_endpoint = match identity.bind_endpoint(&machine_config, now_secs()).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            shutdown_relay(relay_server).await?;
            return Err(error);
        }
    };
    let attachments = machine::AttachmentManager::new(
        machine_config.max_attached_agents,
        machine_config.max_agent_fanout,
        machine_config.query_timeout,
    )
    .with_relay_grant_issuer(identity.clone(), local_relay_audience)
    .with_metrics(metrics.clone());
    let queries = machine::QueryManager::new(
        machine_config.max_pending_queries,
        machine_config.max_offers_per_query,
        machine_config.query_timeout,
    )
    .with_metrics(metrics.clone());
    let placement = machine::PlacementHandler::new(
        machine_config.max_pending_queries,
        machine_config.query_timeout,
    );
    let locations = machine::LocationRegistry::new();
    let mut scheduler_gossip = match machine::SchedulerGossip::start_with_metrics(
        machine::SchedulerGossipServices {
            endpoint: machine_endpoint.clone(),
            attachments: attachments.handler(),
            offers: queries.offer_handler(),
            placement: placement.clone(),
            locations: locations.clone(),
            member_issuers,
            lookup: identity.peer_lookup(),
        },
        &machine_config,
        metrics.clone(),
    )
    .await
    {
        Ok(gossip) => gossip,
        Err(error) => {
            shutdown_machine_endpoint(&machine_endpoint).await?;
            shutdown_relay(relay_server).await?;
            return Err(error);
        }
    };
    let api_identity = identity.clone();
    let (capacity_service, mut capacity_coordinator) = machine::CapacityCoordinator::start(
        identity,
        machine_endpoint.clone(),
        queries,
        attachments.clone(),
        &scheduler_gossip,
        &machine_config,
    );
    placement.install(capacity_service.clone())?;
    let peer_control_relay = machine::PeerControlRelay::new(
        machine_endpoint.clone(),
        scheduler_gossip.members(),
        locations,
        api_identity.clone(),
        clientapi::CLIENT_RELAY_TIMEOUT,
    );
    // Locating an agent goes over gossip, which only exists once the router is
    // accepting, so the publisher is handed over after startup.
    peer_control_relay.install_publisher(scheduler_gossip.publisher())?;
    let forwarder = machine::AgentControlForwarder::new(
        machine_endpoint.clone(),
        attachments,
        peer_control_relay,
        clientapi::CLIENT_RELAY_TIMEOUT,
        clientapi::MAX_CONCURRENT_CLIENT_RELAYS,
    );
    scheduler_gossip
        .control_relay()
        .install(forwarder.clone())?;
    let reconciliation = scheduler_gossip.install_reconciliation(
        api_identity.clone(),
        machine_endpoint.clone(),
        machine_config.query_timeout,
    )?;
    log::info!(
        "stateless scheduler listening on {}",
        listener.local_addr()?
    );
    log::info!(
        "scheduler machine relay listening: http={:?} https={:?} qad={:?}",
        relay_server.http_addr(),
        relay_server.https_addr(),
        relay_server.quic_addr()
    );
    log::info!("scheduler Iroh endpoint started: {}", machine_endpoint.id());

    let http_cancellation = cancellation.clone();
    // Started unconditionally: even a scheduler with no configured peers must
    // announce itself, or a mesh it was dialled into cannot learn its address.
    {
        let pins = std::sync::Arc::new(
            machine::PeerPins::load(&config.machine.key_dir, configured_pins)
                .context("load scheduler peer pins")?,
        );
        tokio::spawn(machine::run_peer_discovery(
            machine::PeerDiscovery {
                peer_urls,
                local_endpoint: machine_endpoint.id(),
                members: scheduler_gossip.members(),
                issuers: relay_issuers,
                joiner: scheduler_gossip.peer_joiner(),
                lookup: api_identity.peer_lookup(),
                pins,
                publisher: scheduler_gossip.publisher(),
                identity: api_identity.clone(),
                endpoint: machine_endpoint.clone(),
            },
            cancellation.clone(),
        ));
    }
    let client_api = clientapi::ClientApi::new(
        capacity_service,
        forwarder,
        api_identity,
        machine_endpoint.clone(),
    )
    .with_metrics(metrics)
    .with_reconciliation(reconciliation)
    .with_rate_limit(config.client_rate_limit_per_minute)
    .router();
    // The rate limiter keys on the peer address, so the service has to be built
    // with connect info; without it every limited route would answer 500.
    let http_server = axum::serve(listener, axum_support::with_connect_info(client_api))
        .with_graceful_shutdown(async move { http_cancellation.cancelled().await })
        .into_future();
    tokio::pin!(http_server);

    tokio::select! {
        signal_result = shutdown_signal() => {
            signal_result?;
            cancellation.cancel();
            tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, &mut http_server)
                .await
                .context("scheduler HTTP graceful shutdown timed out")??;
            shutdown_capacity_coordinator(capacity_coordinator).await?;
            shutdown_gossip(scheduler_gossip).await?;
            shutdown_machine_endpoint(&machine_endpoint).await?;
            shutdown_relay(relay_server).await?;
            metrics_runtime.shutdown().await?;
            Ok(())
        }
        scheduler_result = &mut http_server => {
            cancellation.cancel();
            shutdown_capacity_coordinator(capacity_coordinator).await?;
            shutdown_gossip(scheduler_gossip).await?;
            shutdown_machine_endpoint(&machine_endpoint).await?;
            shutdown_relay(relay_server).await?;
            metrics_runtime.shutdown().await?;
            scheduler_result?;
            anyhow::bail!("scheduler HTTP service stopped unexpectedly")
        }
        relay_result = relay_server.join() => {
            cancellation.cancel();
            tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, &mut http_server)
                .await
                .context("scheduler HTTP graceful shutdown timed out")??;
            shutdown_capacity_coordinator(capacity_coordinator).await?;
            shutdown_gossip(scheduler_gossip).await?;
            shutdown_machine_endpoint(&machine_endpoint).await?;
            shutdown_relay(relay_server).await?;
            metrics_runtime.shutdown().await?;
            relay_result
                .map_err(|error| anyhow::anyhow!("scheduler relay supervisor task failed: {error}"))?
                .map_err(|error| anyhow::anyhow!("scheduler relay stopped unexpectedly: {error}"))?;
            anyhow::bail!("scheduler relay stopped unexpectedly")
        }
        gossip_result = scheduler_gossip.join() => {
            cancellation.cancel();
            tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, &mut http_server)
                .await
                .context("scheduler HTTP graceful shutdown timed out")??;
            shutdown_capacity_coordinator(capacity_coordinator).await?;
            shutdown_gossip(scheduler_gossip).await?;
            shutdown_machine_endpoint(&machine_endpoint).await?;
            shutdown_relay(relay_server).await?;
            metrics_runtime.shutdown().await?;
            gossip_result
                .context("scheduler gossip receiver task failed")??;
            anyhow::bail!("scheduler gossip stopped unexpectedly")
        }
        coordinator_result = capacity_coordinator.join() => {
            cancellation.cancel();
            tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, &mut http_server)
                .await
                .context("scheduler HTTP graceful shutdown timed out")??;
            shutdown_capacity_coordinator(capacity_coordinator).await?;
            shutdown_gossip(scheduler_gossip).await?;
            shutdown_machine_endpoint(&machine_endpoint).await?;
            shutdown_relay(relay_server).await?;
            metrics_runtime.shutdown().await?;
            coordinator_result
                .context("capacity coordinator task failed")??;
            anyhow::bail!("capacity coordinator stopped unexpectedly")
        }
    }
}

async fn shutdown_capacity_coordinator(
    coordinator: machine::CapacityCoordinator,
) -> anyhow::Result<()> {
    tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, coordinator.shutdown())
        .await
        .context("capacity coordinator shutdown timed out")??;
    Ok(())
}

async fn shutdown_gossip(gossip: machine::SchedulerGossip) -> anyhow::Result<()> {
    tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, gossip.shutdown())
        .await
        .context("scheduler gossip graceful shutdown timed out")??;
    Ok(())
}

async fn shutdown_machine_endpoint(endpoint: &iroh::Endpoint) -> anyhow::Result<()> {
    tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, endpoint.close())
        .await
        .context("scheduler Iroh endpoint shutdown timed out")?;
    Ok(())
}

async fn shutdown_relay(server: iroh_relay::server::Server) -> anyhow::Result<()> {
    tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, server.shutdown())
        .await
        .context("scheduler relay graceful shutdown timed out")?
        .map_err(|error| anyhow::anyhow!("scheduler relay shutdown failed: {error}"))
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
