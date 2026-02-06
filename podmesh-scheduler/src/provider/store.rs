use std::collections::HashMap;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::ProviderInfo;

pub trait ProviderStore: Send + Sync {
    fn insert_local(&self, manifest_id: String, info: ProviderInfo);
    fn remove_local(&self, manifest_id: &str) -> Option<ProviderInfo>;
    fn list_local_ids(&self) -> Vec<String>;
    fn list_local(&self) -> Vec<ProviderInfo>;
    fn local_count(&self) -> usize;

    fn get_remote(&self, manifest_id: &str) -> Vec<ProviderInfo>;
    fn set_remote(&self, manifest_id: String, providers: Vec<ProviderInfo>);
    fn upsert_remote_provider(
        &self,
        manifest_id: &str,
        provider: ProviderInfo,
        max_per_manifest: usize,
    );
    fn cleanup_expired_remote(&self) -> usize;
    fn remote_stats(&self) -> (usize, usize);

    fn add_pending_query(
        &self,
        manifest_id: &str,
        sender: mpsc::UnboundedSender<Vec<ProviderInfo>>,
    );
    fn take_pending_queries(
        &self,
        manifest_id: &str,
    ) -> Option<Vec<mpsc::UnboundedSender<Vec<ProviderInfo>>>>;
    fn pending_query_count(&self) -> usize;
}

pub struct InMemoryProviderStore {
    local_providers: Mutex<HashMap<String, ProviderInfo>>,
    remote_providers: Mutex<HashMap<String, Vec<ProviderInfo>>>,
    pending_queries: Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Vec<ProviderInfo>>>>>,
}

impl InMemoryProviderStore {
    pub fn new() -> Self {
        Self {
            local_providers: Mutex::new(HashMap::new()),
            remote_providers: Mutex::new(HashMap::new()),
            pending_queries: Mutex::new(HashMap::new()),
        }
    }
}

impl ProviderStore for InMemoryProviderStore {
    fn insert_local(&self, manifest_id: String, info: ProviderInfo) {
        self.local_providers.lock().insert(manifest_id, info);
    }

    fn remove_local(&self, manifest_id: &str) -> Option<ProviderInfo> {
        self.local_providers.lock().remove(manifest_id)
    }

    fn list_local_ids(&self) -> Vec<String> {
        self.local_providers
            .lock()
            .keys()
            .cloned()
            .collect()
    }

    fn list_local(&self) -> Vec<ProviderInfo> {
        self.local_providers.lock().values().cloned().collect()
    }

    fn local_count(&self) -> usize {
        self.local_providers.lock().len()
    }

    fn get_remote(&self, manifest_id: &str) -> Vec<ProviderInfo> {
        self.remote_providers
            .lock()
            .get(manifest_id)
            .cloned()
            .unwrap_or_default()
    }

    fn set_remote(&self, manifest_id: String, providers: Vec<ProviderInfo>) {
        self.remote_providers.lock().insert(manifest_id, providers);
    }

    fn upsert_remote_provider(
        &self,
        manifest_id: &str,
        provider: ProviderInfo,
        max_per_manifest: usize,
    ) {
        let mut remote_providers = self.remote_providers.lock();
        let providers = remote_providers
            .entry(manifest_id.to_string())
            .or_insert_with(Vec::new);

        if let Some(existing) = providers
            .iter_mut()
            .find(|entry| entry.peer_id == provider.peer_id)
        {
            existing.update_last_seen();
            return;
        }

        if providers.len() >= max_per_manifest {
            if let Some(oldest_idx) = providers
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(idx, _)| idx)
            {
                providers.remove(oldest_idx);
            }
        }

        providers.push(provider);
    }

    fn cleanup_expired_remote(&self) -> usize {
        let mut removed_count = 0;
        let mut remote_providers = self.remote_providers.lock();
        for providers in remote_providers.values_mut() {
            let original_len = providers.len();
            providers.retain(|provider| !provider.is_expired());
            removed_count += original_len - providers.len();
        }
        remote_providers.retain(|_, providers| !providers.is_empty());
        removed_count
    }

    fn remote_stats(&self) -> (usize, usize) {
        let remote_providers = self.remote_providers.lock();
        let manifests = remote_providers.len();
        let total_providers = remote_providers.values().map(|v| v.len()).sum();
        (manifests, total_providers)
    }

    fn add_pending_query(
        &self,
        manifest_id: &str,
        sender: mpsc::UnboundedSender<Vec<ProviderInfo>>,
    ) {
        let mut pending_queries = self.pending_queries.lock();
        pending_queries
            .entry(manifest_id.to_string())
            .or_insert_with(Vec::new)
            .push(sender);
    }

    fn take_pending_queries(
        &self,
        manifest_id: &str,
    ) -> Option<Vec<mpsc::UnboundedSender<Vec<ProviderInfo>>>> {
        self.pending_queries.lock().remove(manifest_id)
    }

    fn pending_query_count(&self) -> usize {
        self.pending_queries.lock().len()
    }
}
