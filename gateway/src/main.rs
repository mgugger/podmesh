use clap::Parser;
use tracing_subscriber::EnvFilter;

mod gw_raft;

#[derive(Parser, Clone, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Opt {
    #[clap(long, default_value = "1")]
    pub id: u64,

    #[clap(long, default_value = "/run/beemesh/gateway_testpod.sock")]
    pub http_addr: String,

    #[clap(long, default_value = "/run/beemesh/host.sock")]
    pub host_socket: String,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_thread_ids(true)
        .with_level(true)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Parse the parameters passed by arguments.
    let options = Opt::parse();

    // Start raft node and obtain shared app state when raft feature is enabled.
    #[cfg(feature = "raft")]
    let app_state = gw_raft::start_example_raft_node(options.id, options.http_addr.clone(), options.host_socket).await?;

    let router = {
        let base = axum::Router::new()
            // health (always present)
            .route("/health", axum::routing::get(gw_raft::network::api::health_check))
            .layer(tower_http::trace::TraceLayer::new_for_http());

        #[cfg(feature = "raft")]
        {
            let raft_routes = gw_raft::build_router(app_state.clone());
            base.merge(raft_routes)
        }

        #[cfg(not(feature = "raft"))]
        {
            base
        }
    };

    if !options.http_addr.is_empty() && options.http_addr.starts_with('/') {
        let _ = std::fs::remove_file(&options.http_addr); // Remove stale socket if present
        match tokio::net::UnixListener::bind(&options.http_addr) {
            Ok(listener) => {
                if let Err(e) = axum::serve(listener, router.into_make_service()).await {
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