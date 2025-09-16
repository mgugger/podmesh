#![allow(clippy::uninlined_format_args)]
#![deny(unused_qualifications)]

mod store;
mod network_v1_http;
mod mem_log;
pub mod network;
mod app;
use network::management;
use network::raft;

use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use openraft::Config;
use crate::gw_raft::store::{Request, Response};

#[cfg(test)]
mod test;

pub type NodeId = u64;

openraft::declare_raft_types!(
    /// Declare the type configuration for example K/V store.
    pub TypeConfig:
        D = Request,
        R = Response,
);

pub type LogStore = store::LogStore;
pub type StateMachineStore = store::StateMachineStore;
pub type Raft = openraft::Raft<TypeConfig>;

#[allow(dead_code)]
#[path = "./utils/declare_types.rs"]
pub mod typ;

pub async fn start_example_raft_node(
    node_id: NodeId,
    http_addr: String,
    host_socket: String,
) -> std::io::Result<Arc<App>> {
    // Create a configuration for the raft instance.
    let config = Config {
        heartbeat_interval: 500,
        election_timeout_min: 1500,
        election_timeout_max: 3000,
        ..Default::default()
    };

    let config = Arc::new(config.validate().unwrap());

    // Create a instance of where the Raft logs will be stored.
    let log_store = LogStore::default();
    // Create a instance of where the Raft data will be stored.
    let state_machine_store = Arc::new(StateMachineStore::default());

    // Create the network layer that will connect and communicate the raft instances and
    // will be used in conjunction with the store created above.
    let mut network = network_v1_http::NetworkFactory::new(host_socket.clone());
    let mut event_rx = network.take_event_receiver().unwrap();

    // Create a local raft instance.
    let raft = openraft::Raft::new(
        node_id,
        config.clone(),
        network,
        log_store.clone(),
        state_machine_store.clone(),
    )
    .await
    .unwrap();

    // Create an application that will store all the instances created above, this will
    // later be used on the actix-web services.
    let app_data = Arc::new(App {
        id: node_id,
        addr: http_addr.clone(),
        raft,
        state_machine_store,
    });

    // Spawn a task to receive unsolicited events from host and log them for now.
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            tracing::debug!("received host event: {:?}", ev);
            // TODO: dispatch event into application (e.g., to App or actors)
        }
    });

    // Do not start the HTTP server here. The binary (main.rs) will create the axum Router
    // and start the server so additional gateway-specific routes can be mounted there.
    Ok(app_data)
}

use app::App;