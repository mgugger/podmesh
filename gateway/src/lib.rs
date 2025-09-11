#![allow(clippy::uninlined_format_args)]
#![deny(unused_qualifications)]

use std::sync::Arc;

use actix_web::middleware;
use actix_web::middleware::Logger;
use actix_web::web::Data;
use actix_web::HttpServer;
use openraft::Config;

use crate::app::App;
use crate::network::api;
use crate::network::management;
use crate::network::raft;
use crate::store::Request;
use crate::store::Response;

pub mod network_v1_http;
pub mod app;
pub mod network;
pub mod store;
#[cfg(test)]
mod test;
mod mem_log;

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

#[path = "./utils/declare_types.rs"]
pub mod typ;

pub async fn start_example_raft_node(
    node_id: NodeId,
    http_addr: String,
    host_socket: String,
) -> std::io::Result<()> {
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
    let app_data = Data::new(App {
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

    // Start the actix-web server.
    let server = HttpServer::new(move || {
        actix_web::App::new()
            .wrap(Logger::default())
            .wrap(Logger::new("%a %t \"%r\" %s %b \"%{User-Agent}i\" %T"))
            .wrap(middleware::Compress::default())
            .app_data(app_data.clone())
            // raft internal RPC
            .service(raft::append)
            .service(raft::snapshot)
            .service(raft::vote)
            // admin API
            .service(management::init)
            .service(management::add_learner)
            .service(management::change_membership)
            .service(management::metrics)
            // application API
            .service(api::write)
            .service(api::read)
            .service(api::linearizable_read)
            // health
            .service(api::health_check)
    });

    // Bind Actix to the address provided in `http_addr`. If `http_addr` looks like a
    // unix socket path (starts with '/'), bind to the UDS file. Otherwise treat it
    // as a TCP listen address. `host_socket` is reserved for connecting to the host
    // agent and is not used for Actix binding.
    if !http_addr.is_empty() && http_addr.starts_with('/') {
        use std::os::unix::net::UnixListener as StdUnixListener;

        let std_listener = StdUnixListener::bind(&http_addr)?;
        std_listener.set_nonblocking(true)?;

        let server = server.listen_uds(std_listener)?;
        server.run().await
    } else {
        let x = server.bind(http_addr)?;
        x.run().await
    }
}