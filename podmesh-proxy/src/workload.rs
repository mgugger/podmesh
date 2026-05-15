use std::net::SocketAddr;

use anyhow::Result;
use tokio::{sync::watch, task::JoinHandle};
use log::info;

use crate::{config::Config, ingress, p2p, restapi};

const DEFAULT_INGRESS_PORT: u16 = 8080;

pub struct Workload {
    cfg: Config,
    p2p_node: Option<p2p::P2pNodeHandle>,
    rest_handle: Option<JoinHandle<()>>,
    peer_id: Option<String>,
    ingress: Option<ingress::IngressServer>,
}

impl Workload {
    pub fn new(cfg: Config) -> Result<Self> {
        Ok(Self {
            cfg,
            p2p_node: None,
            rest_handle: None,
            peer_id: None,
            ingress: None,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        if self.p2p_node.is_some() {
            return Ok(());
        }

        let node = p2p::spawn(&self.cfg)?;
        let peer_rx = node.peer_rx();
        let proxy_client = node.proxy_client();
        let peer_id = node.peer_id().to_string();
        let cert_store = node.cert_store();
        let cert_announce_tx = node.cert_announce_tx();
        let handshake_cert_slot = node.handshake_cert_slot();
        let rest_handle = if self.cfg.disable_rest_api {
            info!(
                "workload rest api disabled host={} port={}",
                self.cfg.rest_host, self.cfg.rest_port
            );
            None
        } else {
            Some(restapi::spawn_rest_server(restapi::RestServerOptions {
                host: self.cfg.rest_host.clone(),
                port: self.cfg.rest_port,
                peer_rx,
                local_peer_id: peer_id.clone(),
                cert_store,
                cert_announce_tx,
                handshake_cert_slot,
            })?)
        };

        self.rest_handle = rest_handle;
        self.p2p_node = Some(node);
        self.peer_id = Some(peer_id);

        if self.cfg.enable_ingress {
            let ingress_server = ingress::IngressServer::spawn(
                self.cfg.rest_host.clone(),
                DEFAULT_INGRESS_PORT,
                ingress::proxy_sidecar_client(proxy_client),
            )?;
            self.ingress = Some(ingress_server);
        } else {
            info!("workload ingress disabled");
        }
        Ok(())
    }

    pub async fn close(&mut self) {
        if let Some(handle) = self.rest_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(ingress_handle) = self.ingress.take() {
            ingress_handle.shutdown().await;
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

    pub fn ingress_address(&self) -> Option<SocketAddr> {
        self.ingress.as_ref().map(|server| server.listen_addr())
    }

    /// Direct access to the cert store for in-process tests that bypass the REST API.
    pub fn cert_store(&self) -> Option<crate::restapi::CertStore> {
        self.p2p_node.as_ref().map(|n| n.cert_store())
    }

    /// Direct access to the cert announce channel for in-process tests.
    pub fn cert_announce_tx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<crate::restapi::CertAnnouncement>> {
        self.p2p_node.as_ref().map(|n| n.cert_announce_tx())
    }

    /// Direct access to the handshake cert slot.
    pub fn handshake_cert_slot(&self) -> Option<p2p::handshake::ProxyCertProvider> {
        self.p2p_node.as_ref().map(|n| n.handshake_cert_slot())
    }

    /// Direct access to the in-memory routing table populated by sidecar registrations.
    /// Useful for integration tests asserting that a verified registration was stored.
    pub fn routing_table_handle(&self) -> Option<p2p::RoutingTable> {
        self.p2p_node.as_ref().map(|n| n.routing_table.clone())
    }
}
