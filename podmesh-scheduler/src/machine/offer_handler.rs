use std::time::Duration;

use anyhow::{Context, Result};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use protocol::{CapacityOffer, MAX_CAPACITY_MESSAGE_BYTES};

use super::QueryManager;

const OFFER_ACK: &[u8] = b"ok";

#[derive(Clone)]
pub struct CapacityOfferHandler {
    manager: QueryManager,
    operation_timeout: Duration,
}

impl CapacityOfferHandler {
    pub(super) fn new(manager: QueryManager, operation_timeout: Duration) -> Self {
        Self {
            manager,
            operation_timeout,
        }
    }

    async fn accept_inner(&self, connection: Connection) -> Result<()> {
        let remote_id = connection.remote_id();
        let (mut send, mut recv) =
            tokio::time::timeout(self.operation_timeout, connection.accept_bi())
                .await
                .context("capacity offer stream timed out")?
                .context("accept capacity offer stream")?;
        let bytes = tokio::time::timeout(
            self.operation_timeout,
            recv.read_to_end(MAX_CAPACITY_MESSAGE_BYTES),
        )
        .await
        .context("capacity offer read timed out")?
        .context("read capacity offer")?;
        let offer = CapacityOffer::from_bytes(&bytes, now_secs())?;
        self.manager
            .submit_offer(offer, remote_id, now_secs())
            .await?;
        send.write_all(OFFER_ACK)
            .await
            .context("write capacity offer acknowledgement")?;
        send.finish()
            .context("finish capacity offer acknowledgement")?;
        let _ = tokio::time::timeout(self.operation_timeout, connection.closed()).await;
        Ok(())
    }
}

impl std::fmt::Debug for CapacityOfferHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CapacityOfferHandler").finish()
    }
}

impl ProtocolHandler for CapacityOfferHandler {
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
