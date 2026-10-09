#![forbid(unsafe_code)]

use std::net::SocketAddr;

use tokio_util::sync::CancellationToken;

use crate::{
    ComponentName, Metrics,
    config::MetricsConfig,
    listener::{ListenerError, MetricsListener},
};

#[derive(Debug)]
pub struct MetricsRuntime {
    metrics: Metrics,
    listener: Option<MetricsListener>,
}

impl MetricsRuntime {
    pub async fn start(
        component: ComponentName,
        config: MetricsConfig,
        cancellation: CancellationToken,
    ) -> Result<Self, ListenerError> {
        let Some(listen) = config.listen else {
            return Ok(Self {
                metrics: Metrics::noop(),
                listener: None,
            });
        };

        let metrics = Metrics::registered(component);
        let listener = MetricsListener::start(listen, metrics.clone(), cancellation).await?;
        Ok(Self {
            metrics,
            listener: Some(listener),
        })
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.as_ref().map(MetricsListener::local_addr)
    }

    pub async fn shutdown(self) -> Result<(), ListenerError> {
        if let Some(listener) = self.listener {
            listener.shutdown().await?;
        }
        Ok(())
    }
}
