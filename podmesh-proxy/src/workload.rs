use std::net::SocketAddr;

use anyhow::Result;
use log::info;
use tokio::{sync::watch, task::JoinHandle};

use crate::{config::Config, ingress, iroh_runtime, restapi};

const DEFAULT_INGRESS_PORT: u16 = 8080;

pub struct Workload {
    cfg: Config,
    p2p_node: Option<iroh_runtime::IrohNodeHandle>,
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

    /// Relay credentials to publish over REST, if the operator opted in and the
    /// proxy actually runs a workload relay.
    fn relay_bootstrap(&self) -> Option<restapi::WorkloadRelayBootstrap> {
        if !self.cfg.publish_relay_bootstrap {
            return None;
        }
        let relay = self.cfg.workload_relay.as_ref()?;
        Some(restapi::WorkloadRelayBootstrap {
            auth_token: relay.auth_token.clone(),
            ca_certificate_der: self.cfg.workload_relay_certificate_der.clone(),
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.p2p_node.is_some() {
            return Ok(());
        }

        let node = iroh_runtime::spawn(&self.cfg).await?;
        let peer_rx = node.peer_rx();
        let proxy_client = node.proxy_client();
        let peer_id = node.peer_id().to_string();
        let endpoint_record = node.endpoint_record_handle();
        let grant_store = node.grant_store();
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
                endpoint_record,
                grant_store,
                relay_bootstrap: self.relay_bootstrap(),
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

    pub fn endpoint_record(&self) -> Option<protocol::EndpointRecord> {
        self.p2p_node
            .as_ref()
            .and_then(|node| node.endpoint_record().ok())
    }

    pub fn network_ready_rx(&self) -> Option<watch::Receiver<bool>> {
        self.p2p_node.as_ref().map(|node| node.network_ready_rx())
    }

    pub fn ingress_address(&self) -> Option<SocketAddr> {
        self.ingress.as_ref().map(|server| server.listen_addr())
    }

    /// Direct access to the cert store for in-process tests that bypass the REST API.
    pub fn grant_store(&self) -> Option<crate::restapi::ProxyGrantStore> {
        self.p2p_node.as_ref().map(|n| n.grant_store())
    }

    /// Direct access to the in-memory routing table populated by sidecar registrations.
    /// Useful for integration tests asserting that a verified registration was stored.
    pub fn routing_table_handle(&self) -> Option<iroh_runtime::RoutingTable> {
        self.p2p_node.as_ref().map(|node| node.routing_table())
    }
}
