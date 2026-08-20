use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::Result;
use iroh::Endpoint;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{
    AgentControlForwarder, GossipPublisher, MemberRegistry, ReconciliationOutcome,
    ReconciliationRegistry, ReconciliationResponder, SchedulerIdentity,
};

#[derive(Clone)]
pub struct ReconciliationService {
    identity: SchedulerIdentity,
    endpoint: Endpoint,
    members: MemberRegistry,
    publisher: GossipPublisher,
    registry: ReconciliationRegistry,
    responder: ReconciliationResponder,
    timeout: Duration,
}

impl ReconciliationService {
    pub fn new(
        identity: SchedulerIdentity,
        endpoint: Endpoint,
        members: MemberRegistry,
        publisher: GossipPublisher,
        registry: ReconciliationRegistry,
        forwarder: Arc<OnceCell<AgentControlForwarder>>,
        timeout: Duration,
    ) -> Self {
        let responder = ReconciliationResponder::new(
            identity.clone(),
            endpoint.clone(),
            registry.clone(),
            forwarder,
            timeout,
        );
        Self {
            identity,
            endpoint,
            members,
            publisher,
            registry,
            responder,
            timeout,
        }
    }

    pub async fn reconcile(&self, owner_request: Vec<u8>) -> Result<ReconciliationOutcome> {
        let now = crate::now_secs();
        let query_id = Uuid::new_v4().to_string();
        let mut expected = self.members.snapshot().into_iter().collect::<HashSet<_>>();
        expected.insert(self.endpoint.id());
        let notify = self.registry.begin(query_id.clone(), expected).await?;
        let reply_endpoint = self.identity.endpoint_record(
            &self.endpoint.addr(),
            now,
            now + protocol::MAX_RECONCILIATION_LIFETIME_SECS,
        )?;
        let query = protocol::SchedulerReconciliationQuery {
            version: protocol::RECONCILIATION_PROTOCOL_VERSION,
            query_id: query_id.clone(),
            owner_request,
            reply_endpoint,
            issued_at_secs: now,
            expires_at_secs: now + protocol::MAX_RECONCILIATION_LIFETIME_SECS,
            signing_pubkey: String::new(),
            signature: String::new(),
        }
        .sign(
            self.identity.signing_public(),
            self.identity.signing_private(),
            now,
        )?;

        let local = self.responder.answer_local(query.clone());
        let publish = self.publisher.publish_reconciliation(query);
        let (local_result, publish_result) = tokio::join!(local, publish);
        if let Err(error) = local_result {
            log::warn!("local reconciliation fanout failed: {error:#}");
        }
        if let Err(error) = publish_result {
            self.registry.cancel(&query_id).await;
            return Err(error);
        }
        self.registry.finish(&query_id, notify, self.timeout).await
    }
}
