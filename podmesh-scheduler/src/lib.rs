use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use clap::Parser;
use protocol::AgentAdvertisement;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

const MAX_REGISTERED_AGENTS: usize = 10_000;
const MAX_ADVERTISEMENT_FUTURE_SECS: u64 = 120;
const MAX_REGISTRATION_BODY_BYTES: usize = 64 * 1024;
const MAX_EXCLUDED_AGENTS: usize = 64;

fn now_secs() -> u64 {
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
}

#[derive(Clone, Default)]
pub struct AgentRegistry {
    agents: Arc<RwLock<HashMap<String, AgentAdvertisement>>>,
}

impl AgentRegistry {
    pub async fn register(
        &self,
        advertisement: AgentAdvertisement,
        now: u64,
    ) -> anyhow::Result<()> {
        advertisement.verify(now)?;
        anyhow::ensure!(
            advertisement.expires_at_secs <= now.saturating_add(MAX_ADVERTISEMENT_FUTURE_SECS),
            "advertisement expiry is too far in the future"
        );
        let mut agents = self.agents.write().await;
        agents.retain(|_, value| value.expires_at_secs >= now);
        anyhow::ensure!(
            agents.contains_key(&advertisement.node_id) || agents.len() < MAX_REGISTERED_AGENTS,
            "agent registry is full"
        );
        if let Some(current) = agents.get(&advertisement.node_id) {
            anyhow::ensure!(
                advertisement.expires_at_secs > current.expires_at_secs,
                "advertisement does not advance expiry"
            );
        }
        agents.insert(advertisement.node_id.clone(), advertisement);
        Ok(())
    }

    pub async fn select(&self, excluded: &HashSet<String>, now: u64) -> Option<AgentAdvertisement> {
        let mut agents = self.agents.write().await;
        agents.retain(|_, value| value.expires_at_secs >= now);
        agents
            .values()
            .filter(|value| value.available && !excluded.contains(&value.node_id))
            .min_by(|left, right| {
                left.load_percent
                    .cmp(&right.load_percent)
                    .then_with(|| left.node_id.cmp(&right.node_id))
            })
            .cloned()
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.agents.read().await.len()
    }
}

#[derive(Debug, Deserialize)]
struct SelectionQuery {
    #[serde(default)]
    exclude: String,
}

pub fn router(registry: AgentRegistry) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/agents", post(register_agent))
        .route("/api/v1/agents/select", get(select_agent))
        .layer(DefaultBodyLimit::max(MAX_REGISTRATION_BODY_BYTES))
        .with_state(registry)
}

async fn register_agent(
    State(registry): State<AgentRegistry>,
    Json(advertisement): Json<AgentAdvertisement>,
) -> Result<StatusCode, (StatusCode, String)> {
    registry
        .register(advertisement, now_secs())
        .await
        .map_err(|error| {
            log::warn!("agent registration rejected: {error}");
            (
                StatusCode::BAD_REQUEST,
                "invalid agent advertisement".into(),
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn select_agent(
    State(registry): State<AgentRegistry>,
    Query(query): Query<SelectionQuery>,
) -> Result<Json<AgentAdvertisement>, (StatusCode, String)> {
    let excluded: HashSet<String> = query
        .exclude
        .split(',')
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if excluded.len() > MAX_EXCLUDED_AGENTS || excluded.iter().any(|value| value.len() > 128) {
        return Err((StatusCode::BAD_REQUEST, "invalid exclusion list".into()));
    }
    registry
        .select(&excluded, now_secs())
        .await
        .map(Json)
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no agent available".into()))
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    log::info!(
        "stateless scheduler listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, router(AgentRegistry::default()))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn advertisement(load_percent: u8, expires_at_secs: u64) -> AgentAdvertisement {
        let (public, private) = crypto::ensure_keypair_ephemeral().unwrap();
        AgentAdvertisement {
            version: protocol::AGENT_PROTOCOL_VERSION,
            node_id: String::new(),
            kem_pubkey: crypto::b64_encode(&[7; 32]),
            relay_url: "http://127.0.0.1:3100".into(),
            capabilities: vec!["multi-workload".into()],
            available: true,
            load_percent,
            expires_at_secs,
            nonce: format!("nonce-{load_percent}"),
            signature: String::new(),
        }
        .sign(&public, &private)
        .unwrap()
    }

    #[tokio::test]
    async fn registration_is_ephemeral_and_selection_prefers_low_load() {
        let registry = AgentRegistry::default();
        registry
            .register(advertisement(80, 1_030), 1_000)
            .await
            .unwrap();
        registry
            .register(advertisement(10, 1_030), 1_000)
            .await
            .unwrap();
        let selected = registry.select(&HashSet::new(), 1_000).await.unwrap();
        assert_eq!(selected.load_percent, 10);
        assert_eq!(registry.len().await, 2);
        assert!(registry.select(&HashSet::new(), 1_031).await.is_none());
        assert_eq!(registry.len().await, 0);
    }

    #[tokio::test]
    async fn invalid_signature_is_rejected() {
        let registry = AgentRegistry::default();
        let mut value = advertisement(10, 1_030);
        value.available = false;
        assert!(registry.register(value, 1_000).await.is_err());
    }
}
