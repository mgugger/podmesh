use std::sync::Arc;

use async_trait::async_trait;
use libp2p::kad::RecordKey;
use libp2p::{PeerId, Swarm};
use log::{debug, error};

use crate::podmesh_p2p::behaviour::MyBehaviour;

use super::{ProviderError, ProviderResult};

#[async_trait]
pub trait ProviderNetwork: Send + Sync {
    fn start_providing(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<()>;

    fn get_providers(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<()>;

    fn local_peer_id(&self, swarm: &Swarm<MyBehaviour>) -> PeerId;
}

pub struct DhtProviderNetwork;

impl DhtProviderNetwork {
    pub fn new() -> Arc<dyn ProviderNetwork> {
        Arc::new(Self)
    }

    fn record_key(manifest_id: &str) -> RecordKey {
        RecordKey::new(&format!("provider:{}", manifest_id))
    }
}

#[async_trait]
impl ProviderNetwork for DhtProviderNetwork {
    fn start_providing(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<()> {
        let record_key = Self::record_key(manifest_id);
        match swarm.behaviour_mut().kademlia.start_providing(record_key) {
            Ok(query_id) => {
                debug!(
                    "Started DHT provider announcement for manifest {} (query_id: {:?})",
                    manifest_id, query_id
                );
                Ok(())
            }
            Err(err) => {
                error!(
                    "Failed to start DHT provider announcement for manifest {}: {}",
                    manifest_id, err
                );
                Err(ProviderError::DhtError(format!(
                    "Failed to start providing: {}",
                    err
                )))
            }
        }
    }

    fn get_providers(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<()> {
        let record_key = Self::record_key(manifest_id);
        let query_id = swarm.behaviour_mut().kademlia.get_providers(record_key);
        debug!(
            "Started DHT provider query for manifest {} (query_id: {:?})",
            manifest_id, query_id
        );
        Ok(())
    }

    fn local_peer_id(&self, swarm: &Swarm<MyBehaviour>) -> PeerId {
        *swarm.local_peer_id()
    }
}
