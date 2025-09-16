use clap::Parser;
use tracing_subscriber::EnvFilter;

mod gw_raft;
use gw_raft::start_example_raft_node;


#[derive(Parser, Clone, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Opt {
    #[clap(long, default_value = "1")]
    pub id: u64,

    #[clap(long, default_value = "/run/podmesh/gateway_testpod.sock")]
    pub http_addr: String,

    #[clap(long, default_value = "/run/podmesh/host.sock")]
    pub host_socket: String,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Setup the logger
    tracing_subscriber::fmt()
        .with_target(true)
        .with_thread_ids(true)
        .with_level(true)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Parse the parameters passed by arguments.
    let options = Opt::parse();

    // Start raft node and obtain shared app state.
    let app_state = gw_raft::start_example_raft_node(options.id, options.http_addr.clone(), options.host_socket).await?;

    // Build axum Router with gw_raft routes and additional gateway-specific routes.
    let raft_router = axum::Router::new()
        // raft internal RPC
        .route("/append", axum::routing::post(gw_raft::network::raft::append))
        .route("/snapshot", axum::routing::post(gw_raft::network::raft::snapshot))
        .route("/vote", axum::routing::post(gw_raft::network::raft::vote))
        // admin API
        .route("/management/init", axum::routing::post(gw_raft::network::management::init))
        .route("/management/add_learner", axum::routing::post(gw_raft::network::management::add_learner))
        .route("/management/change_membership", axum::routing::post(gw_raft::network::management::change_membership))
        .route("/management/metrics", axum::routing::post(gw_raft::network::management::metrics))
        // application API
        .route("/write", axum::routing::post(gw_raft::network::api::write))
        .route("/read", axum::routing::post(gw_raft::network::api::read))
        .route("/linearizable_read", axum::routing::post(gw_raft::network::api::linearizable_read))
        // health
        .route("/health", axum::routing::get(gw_raft::network::api::health_check))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::add_extension::AddExtensionLayer::new(app_state.clone()));

    // Serve via Unix Domain Socket given by http_addr
    if !options.http_addr.is_empty() && options.http_addr.starts_with('/') {
        // Remove stale socket if present
        let _ = std::fs::remove_file(&options.http_addr);
        match tokio::net::UnixListener::bind(&options.http_addr) {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, raft_router.into_make_service()).await {
                    eprintln!("axum UDS server error: {}", e);
                }
            }
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("failed to bind UDS: {}", e))),
        }
    } else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "gateway expects UDS socket path in http_addr"));
    }

    Ok(())
}