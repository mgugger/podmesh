use std::time::Duration;

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};

use super::{MemberRegistry, ReconciliationRegistry};

#[derive(Clone)]
pub struct ReconciliationResponseHandler {
    members: MemberRegistry,
    registry: ReconciliationRegistry,
    timeout: Duration,
}

impl std::fmt::Debug for ReconciliationResponseHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReconciliationResponseHandler")
            .finish()
    }
}

impl ReconciliationResponseHandler {
    pub fn new(
        members: MemberRegistry,
        registry: ReconciliationRegistry,
        timeout: Duration,
    ) -> Self {
        Self {
            members,
            registry,
            timeout,
        }
    }
}

impl ProtocolHandler for ReconciliationResponseHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        if !self.members.contains(&remote) {
            connection.close(403u16.into(), b"scheduler membership required");
            return Err(AcceptError::from_err(std::io::Error::other(
                "scheduler membership required",
            )));
        }
        loop {
            let stream = match tokio::time::timeout(self.timeout, connection.accept_bi()).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(_)) | Err(_) => return Ok(()),
            };
            let (mut send, mut recv) = stream;
            let bytes = tokio::time::timeout(
                self.timeout,
                recv.read_to_end(protocol::MAX_RECONCILIATION_RESPONSE_BYTES),
            )
            .await
            .map_err(accept_error)?
            .map_err(accept_error)?;
            let response =
                protocol::SchedulerReconciliationResponse::from_bytes(&bytes, crate::now_secs())
                    .map_err(accept_error)?;
            let responder = response
                .responder_endpoint
                .endpoint_id
                .as_slice()
                .try_into()
                .ok()
                .and_then(|bytes: [u8; protocol::IROH_ENDPOINT_ID_BYTES]| {
                    iroh::EndpointId::from_bytes(&bytes).ok()
                });
            if responder != Some(remote) {
                return Err(accept_error(
                    "reconciliation response signer does not match transport peer",
                ));
            }
            self.registry
                .record(remote, response)
                .await
                .map_err(accept_error)?;
            send.write_all(&[1]).await.map_err(accept_error)?;
            send.finish().map_err(accept_error)?;
        }
    }
}

fn accept_error(error: impl std::fmt::Display) -> AcceptError {
    AcceptError::from_err(std::io::Error::other(error.to_string()))
}
