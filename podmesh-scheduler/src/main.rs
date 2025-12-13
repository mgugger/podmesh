use clap::Parser;
use podmesh_scheduler::{Cli, start_machine};
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let handles = start_machine(cli).await?;
    if !handles.is_empty() {
        // Wait for either SIGTERM or SIGINT to gracefully shutdown
        tokio::select! {
            _ = signal::ctrl_c() => {
                log::info!("received SIGINT, shutting down");
            }
            _ = async {
                #[cfg(unix)]
                {
                    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                        .expect("failed to install SIGTERM handler");
                    sigterm.recv().await;
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {
                log::info!("received SIGTERM, shutting down");
            }
        }
        // Abort all spawned tasks
        for handle in handles {
            handle.abort();
        }
    }
    Ok(())
}
