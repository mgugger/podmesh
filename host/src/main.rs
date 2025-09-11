use clap::Parser;

/// Podmesh Host Agent
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Host address for REST API
    #[arg(long, default_value = "127.0.0.1")]
    api_host: String,

    /// Port for REST API
    #[arg(long, default_value = "3000")]
    api_port: u16,

    /// Disable REST API server
    #[arg(long, default_value_t = false)]
    disable_rest_api: bool,

    /// Custom node name (optional)
    #[arg(long)]
    node_name: Option<String>,

    /// Libp2p base port (optional)
    #[arg(long)]
    libp2p_port: Option<u16>,
}
mod libp2pmod;
mod podman;
mod restapi;
mod pod_communication;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let (swarm, topic, peer_rx, peer_tx) = libp2pmod::setup_libp2p_node()?;

    let libp2p_handle = tokio::spawn(async move {
        if let Err(e) = libp2pmod::start_libp2p_node(swarm, topic, peer_tx).await {
            eprintln!("libp2p node error: {}", e);
        }
    });

    let axum_handle = if !cli.disable_rest_api {
        let app = restapi::build_router(peer_rx).merge(podman::build_router());
        let bind_addr = format!("{}:{}", cli.api_host, cli.api_port);
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .unwrap();
        println!("listening on {}", listener.local_addr().unwrap());
        Some(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app.into_make_service()).await {
                eprintln!("axum server error: {}", e);
            }
        }))
    } else {
        println!("REST API disabled");
        None
    };

    // Wait for both tasks (if REST API enabled)
    if let Some(axum_handle) = axum_handle {
        let _ = tokio::try_join!(libp2p_handle, axum_handle);
    } else {
        let _ = libp2p_handle.await;
    }
    Ok(())
}
