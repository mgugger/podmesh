use anyhow::Result;
use tokio::{sync::watch, task::JoinHandle};
use tracing::info;

use crate::{config::Config, p2p, restapi};

pub struct Workload {
    cfg: Config,
    p2p_node: Option<p2p::P2pNodeHandle>,
    rest_handle: Option<JoinHandle<()>>,
    peer_id: Option<String>,
}

impl Workload {
    pub fn new(cfg: Config) -> Result<Self> {
        Ok(Self {
            cfg,
            p2p_node: None,
            rest_handle: None,
            peer_id: None,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        if self.p2p_node.is_some() {
            return Ok(());
        }

        let node = p2p::spawn(&self.cfg)?;
        let peer_rx = node.peer_rx();
        let peer_id = node.peer_id().to_string();
        let rest_handle = if self.cfg.disable_rest_api {
            info!(
                host = %self.cfg.rest_host,
                port = self.cfg.rest_port,
                "workload rest api disabled"
            );
            None
        } else {
            Some(restapi::spawn_rest_server(
                self.cfg.rest_host.clone(),
                self.cfg.rest_port,
                peer_rx,
            )?)
        };

        self.rest_handle = rest_handle;
        self.p2p_node = Some(node);
        self.peer_id = Some(peer_id);
        Ok(())
    }

    pub async fn close(&mut self) {
        if let Some(handle) = self.rest_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(node) = self.p2p_node.take() {
            node.shutdown().await;
        }
    }

    pub fn peer_id(&self) -> Option<&str> {
        self.peer_id.as_deref()
    }

    pub fn kad_bootstrap_rx(&self) -> Option<watch::Receiver<bool>> {
        self.p2p_node.as_ref().map(|node| node.kad_ready_rx())
    }

    pub fn proxy_provider_announced_rx(&self) -> Option<watch::Receiver<bool>> {
        self.p2p_node
            .as_ref()
            .map(|node| node.proxy_provider_announced_rx())
    }
}
