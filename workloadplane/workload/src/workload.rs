use anyhow::Result;
use tokio::task::JoinHandle;

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
        let rest = restapi::spawn_rest_server(
            self.cfg.rest_host.clone(),
            self.cfg.rest_port,
            peer_rx,
        )?;

        self.rest_handle = Some(rest);
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
}
