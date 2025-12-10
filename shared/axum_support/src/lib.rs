use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use axum::Router;
use log::{info, warn};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub mod rate_limiter;
pub use rate_limiter::{create_rate_limiter, rate_limit_middleware, RateLimiterState};

/// Spawn a TCP axum server by binding to the provided address inside the task.
/// This version supports ConnectInfo<SocketAddr> for rate limiting middleware.
pub fn spawn_tcp_server(
    addr: SocketAddr,
    router: Router,
    label: impl Into<String>,
) -> JoinHandle<()> {
    let label = label.into();
    tokio::spawn(async move {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let addr = listener
                    .local_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "unknown".into());
                info!(
                    target: "axum_support",
                    "axum tcp server listening (label={}, addr={})",
                    label,
                    addr
                );
                // Use into_make_service_with_connect_info to provide ConnectInfo<SocketAddr>
                // for rate limiting middleware
                if let Err(err) = axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    warn!(
                        target: "axum_support",
                        "axum tcp server stopped (label={}, err={})",
                        label,
                        err
                    );
                }
            }
            Err(err) => {
                warn!(
                    target: "axum_support",
                    "failed to bind tcp listener (label={}, err={})",
                    label,
                    err
                );
            }
        }
    })
}

/// Spawn an axum server on an already-bound TCP listener.
/// This version supports ConnectInfo<SocketAddr> for rate limiting middleware.
pub fn spawn_tcp_listener(
    listener: TcpListener,
    router: Router,
    label: impl Into<String>,
) -> JoinHandle<()> {
    let label = label.into();
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());

    tokio::spawn(async move {
        info!(
            target: "axum_support",
            "axum tcp server listening (label={}, addr={})",
            label,
            addr
        );
        // Use into_make_service_with_connect_info to provide ConnectInfo<SocketAddr>
        // for rate limiting middleware
        if let Err(err) = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            warn!(
                target: "axum_support",
                "axum tcp server stopped (label={}, err={})",
                label,
                err
            );
        }
    })
}

/// Start an axum server bound to a Unix Domain Socket path.
#[cfg(unix)]
pub async fn start_unix_server(
    path: impl AsRef<Path>,
    router: Router,
    label: impl Into<String>,
) -> Result<JoinHandle<()>> {
    let path = path.as_ref();
    if path.exists() {
        tokio::fs::remove_file(path).await.ok();
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let listener = UnixListener::bind(path)?;
    Ok(spawn_unix_listener(listener, router, label))
}

#[cfg(unix)]
pub fn spawn_unix_listener(
    listener: UnixListener,
    router: Router,
    label: impl Into<String>,
) -> JoinHandle<()> {
    let label = label.into();
    let path = listener
        .local_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "unix://unknown".to_string());

    tokio::spawn(async move {
        info!(
            target: "axum_support",
            "axum unix server listening (label={}, path={})",
            label,
            path
        );
        if let Err(err) = axum::serve(listener, router).await {
            warn!(
                target: "axum_support",
                "axum unix server stopped (label={}, err={})",
                label,
                err
            );
        }
    })
}

/// Helper to create a socket address from host/port pairs.
pub fn parse_socket_addr(host: &str, port: u16) -> Result<SocketAddr> {
    Ok(format!("{}:{}", host, port).parse()?)
}
