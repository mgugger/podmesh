use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use protocol::{MAX_CAPACITY_MESSAGE_BYTES, PlacementError, PlacementRequest, PlacementResponse};
use tokio::sync::Semaphore;

use super::{CapacityCriteria, CapacityService};

#[derive(Clone)]
pub struct PlacementHandler {
    service: Arc<OnceLock<CapacityService>>,
    permits: Arc<Semaphore>,
    operation_timeout: Duration,
}

impl PlacementHandler {
    pub fn new(max_concurrent: usize, operation_timeout: Duration) -> Self {
        Self {
            service: Arc::new(OnceLock::new()),
            permits: Arc::new(Semaphore::new(max_concurrent)),
            operation_timeout,
        }
    }

    pub fn install(&self, service: CapacityService) -> Result<()> {
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("placement capacity service was already installed"))
    }

    async fn accept_inner(&self, connection: Connection) -> Result<()> {
        let (mut send, mut recv) =
            tokio::time::timeout(self.operation_timeout, connection.accept_bi())
                .await
                .context("placement request stream timed out")?
                .context("accept placement request stream")?;
        let bytes = tokio::time::timeout(
            self.operation_timeout,
            recv.read_to_end(MAX_CAPACITY_MESSAGE_BYTES),
        )
        .await
        .context("placement request read timed out")?
        .context("read placement request")?;
        let now = now_secs();
        let request = PlacementRequest::from_bytes(&bytes, now)?;
        let response = match self.permits.clone().try_acquire_owned() {
            Ok(_permit) => match self.service.get() {
                Some(service) => {
                    let criteria = CapacityCriteria {
                        cpu_milli: request.cpu_milli,
                        memory_bytes: request.memory_bytes,
                        storage_bytes: request.storage_bytes,
                        required_capabilities: request.required_capabilities.clone(),
                        excluded_endpoint_ids: request.excluded_endpoint_ids.clone(),
                    };
                    match service.solicit(criteria).await {
                        Ok(Some(offer)) => {
                            PlacementResponse::selected(request.request_id.clone(), offer)
                        }
                        Ok(None) => PlacementResponse::failed(
                            request.request_id.clone(),
                            PlacementError::NoCapacity,
                        ),
                        Err(error) => {
                            log::error!("capacity solicitation failed: {error}");
                            PlacementResponse::failed(
                                request.request_id.clone(),
                                PlacementError::Internal,
                            )
                        }
                    }
                }
                None => PlacementResponse::failed(request.request_id.clone(), PlacementError::Busy),
            },
            Err(_) => PlacementResponse::failed(request.request_id.clone(), PlacementError::Busy),
        };
        let encoded = response.to_bytes(now_secs())?;
        send.write_all(&encoded)
            .await
            .context("write placement response")?;
        send.finish().context("finish placement response")?;
        let _ = tokio::time::timeout(self.operation_timeout, connection.closed()).await;
        Ok(())
    }
}

impl std::fmt::Debug for PlacementHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PlacementHandler").finish()
    }
}

impl ProtocolHandler for PlacementHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.accept_inner(connection)
            .await
            .map_err(|error| AcceptError::from_err(std::io::Error::other(error.to_string())))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod placement_tests;
