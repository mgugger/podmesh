//! Provider announcement system for manifest hosting
//!
//! This module provides functionality for nodes to announce themselves as providers
//! of specific manifests and for other nodes to discover which nodes are hosting
//! which manifests. This is more efficient than using gossipsub for discovery.

use libp2p::{PeerId, Swarm};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

use crate::podmesh_p2p::behaviour::MyBehaviour;

pub mod store;
pub mod network;

pub mod announcements;
pub mod discovery;

use async_trait::async_trait;
use network::{DhtProviderNetwork, ProviderNetwork};
use store::{InMemoryProviderStore, ProviderStore};

/// Errors that can occur during provider operations
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("DHT error: {0}")]
    DhtError(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("Timeout waiting for providers")]
    Timeout,
}

/// Result type for provider operations
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Information about a manifest provider
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// The peer ID of the provider
    pub peer_id: PeerId,
    /// The manifest ID being provided
    pub manifest_id: String,
    /// When this provider was first announced
    pub announced_at: SystemTime,
    /// When this provider was last seen
    pub last_seen: SystemTime,
    /// TTL for this provider announcement (in seconds)
    pub ttl_seconds: u64,
    /// Additional metadata about the provider
    pub metadata: HashMap<String, String>,
}

impl ProviderInfo {
    /// Check if this provider announcement has expired
    pub fn is_expired(&self) -> bool {
        if let Ok(elapsed) = self.announced_at.elapsed() {
            elapsed.as_secs() > self.ttl_seconds
        } else {
            true // If we can't determine elapsed time, consider expired
        }
    }

    /// Update the last seen timestamp
    pub fn update_last_seen(&mut self) {
        self.last_seen = SystemTime::now();
    }
}

/// Manager for provider announcements and discovery
pub struct ProviderManager {
    /// Storage backend for provider data
    store: Arc<dyn ProviderStore>,
    /// Network backend for DHT interactions
    network: Arc<dyn ProviderNetwork>,
    /// Configuration
    config: ProviderConfig,
}

/// Configuration for the provider manager
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Default TTL for provider announcements (in seconds)
    pub default_ttl_seconds: u64,
    /// How often to re-announce local providers (in seconds)
    pub reannounce_interval_seconds: u64,
    /// How often to clean up expired providers (in seconds)
    pub cleanup_interval_seconds: u64,
    /// Maximum number of providers to track per manifest
    pub max_providers_per_manifest: usize,
    /// Timeout for provider discovery queries (in seconds)
    pub discovery_timeout_seconds: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_ttl_seconds: 3600,         // 1 hour
            reannounce_interval_seconds: 1800, // 30 minutes
            cleanup_interval_seconds: 300,     // 5 minutes
            max_providers_per_manifest: 50,
            discovery_timeout_seconds: 30,
        }
    }
}

impl ProviderManager {
    /// Create a new provider manager
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            store: Arc::new(InMemoryProviderStore::new()),
            network: DhtProviderNetwork::new(),
            config,
        }
    }

    /// Create a provider manager with default configuration
    pub fn new_default() -> Self {
        Self::new(ProviderConfig::default())
    }

    /// Announce that this node is providing a manifest
    pub fn announce_provider(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
        metadata: HashMap<String, String>,
    ) -> ProviderResult<()> {
        let local_peer_id = self.network.local_peer_id(swarm);

        info!(
            "Announcing provider for manifest: {} from peer: {}",
            manifest_id, local_peer_id
        );

        // Create provider info
        let provider_info = ProviderInfo {
            peer_id: local_peer_id,
            manifest_id: manifest_id.to_string(),
            announced_at: SystemTime::now(),
            last_seen: SystemTime::now(),
            ttl_seconds: self.config.default_ttl_seconds,
            metadata,
        };

        // Store locally
        self.store
            .insert_local(manifest_id.to_string(), provider_info);

        // Announce via DHT
        self.network.start_providing(swarm, manifest_id)?;

        debug!(
            "Successfully announced provider for manifest: {}",
            manifest_id
        );
        Ok(())
    }

    /// Stop providing a manifest
    pub fn stop_providing(&self, manifest_id: &str) -> ProviderResult<()> {
        info!(
            "Stopping provider announcement for manifest: {}",
            manifest_id
        );

        if self.store.remove_local(manifest_id).is_some() {
            debug!("Removed local provider for manifest: {}", manifest_id);
            Ok(())
        } else {
            warn!(
                "Attempted to stop providing manifest {} but it wasn't being provided",
                manifest_id
            );
            Err(ProviderError::ProviderNotFound(manifest_id.to_string()))
        }
    }

    /// Discover providers for a manifest
    pub async fn discover_providers(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<Vec<ProviderInfo>> {
        debug!("Discovering providers for manifest: {}", manifest_id);

        // Check if we have cached providers
        let cached_providers = self.store.get_remote(manifest_id);
        if !cached_providers.is_empty() {
            let valid_providers: Vec<ProviderInfo> = cached_providers
                .into_iter()
                .filter(|provider| !provider.is_expired())
                .collect();

            if !valid_providers.is_empty() {
                debug!(
                    "Found {} cached providers for manifest: {}",
                    valid_providers.len(),
                    manifest_id
                );
                return Ok(valid_providers);
            }
        }

        // Query DHT for providers
        self.query_dht_providers(swarm, manifest_id).await
    }

    /// Get all manifests this node is providing
    pub fn get_local_providers(&self) -> Vec<ProviderInfo> {
        self.store.list_local()
    }

    /// Get providers for a specific manifest (including expired ones)
    pub fn get_providers_for_manifest(&self, manifest_id: &str) -> Vec<ProviderInfo> {
        self.store.get_remote(manifest_id)
    }

    /// Clean up expired providers
    pub fn cleanup_expired_providers(&self) {
        debug!("Cleaning up expired providers");

        let removed_count = self.store.cleanup_expired_remote();

        if removed_count > 0 {
            debug!("Cleaned up {} expired providers", removed_count);
        }
    }

    /// Re-announce all local providers
    pub fn reannounce_local_providers(&self, swarm: &mut Swarm<MyBehaviour>) {
        debug!("Re-announcing local providers");

        let manifest_ids = self.store.list_local_ids();

        let manifest_count = manifest_ids.len();

        for manifest_id in manifest_ids {
            if let Err(e) = self.announce_via_dht(swarm, &manifest_id) {
                warn!(
                    "Failed to re-announce provider for manifest {}: {}",
                    manifest_id, e
                );
            }
        }

        debug!("Re-announced {} local providers", manifest_count);
    }

    /// Start background tasks for provider management
    pub fn start_background_tasks(&self, _swarm: &mut Swarm<MyBehaviour>) {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();

        // Start cleanup task
        {
            let store = Arc::clone(&store);
            let cleanup_interval = Duration::from_secs(config.cleanup_interval_seconds);

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(cleanup_interval);
                loop {
                    interval.tick().await;

                    let removed_count = store.cleanup_expired_remote();

                    if removed_count > 0 {
                        debug!(
                            "Background cleanup: removed {} expired providers",
                            removed_count
                        );
                    }
                }
            });
        }

        info!("Started provider manager background tasks");
    }

    /// Internal method to announce via DHT
    fn announce_via_dht(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<()> {
        self.network.start_providing(swarm, manifest_id)
    }

    /// Internal method to query DHT for providers
    async fn query_dht_providers(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<Vec<ProviderInfo>> {
        // Create a channel to receive results
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Store the sender for this query
        self.store.add_pending_query(manifest_id, tx);

        // Start the DHT query
        self.network.get_providers(swarm, manifest_id)?;

        // Wait for results with timeout
        let timeout = Duration::from_secs(self.config.discovery_timeout_seconds);
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(providers)) => {
                debug!(
                    "Found {} providers for manifest: {}",
                    providers.len(),
                    manifest_id
                );

                // Cache the results
                self.store
                    .set_remote(manifest_id.to_string(), providers.clone());

                Ok(providers)
            }
            Ok(None) => {
                warn!(
                    "Provider query channel closed for manifest: {}",
                    manifest_id
                );
                Ok(Vec::new())
            }
            Err(_) => {
                warn!(
                    "Timeout waiting for providers for manifest: {}",
                    manifest_id
                );
                Err(ProviderError::Timeout)
            }
        }
    }

    /// Handle DHT provider query results (called from libp2p event handler)
    pub fn handle_provider_found(&self, manifest_id: &str, provider_peer: PeerId) {
        debug!(
            "DHT provider found for manifest {}: {}",
            manifest_id, provider_peer
        );

        let provider_info = ProviderInfo {
            peer_id: provider_peer,
            manifest_id: manifest_id.to_string(),
            announced_at: SystemTime::now(),
            last_seen: SystemTime::now(),
            ttl_seconds: self.config.default_ttl_seconds,
            metadata: HashMap::new(),
        };

        // Add to remote providers
        self.store.upsert_remote_provider(
            manifest_id,
            provider_info,
            self.config.max_providers_per_manifest,
        );

        // Notify any pending queries
        if let Some(senders) = self.store.take_pending_queries(manifest_id) {
            let providers = self.get_providers_for_manifest(manifest_id);
            for sender in senders {
                let _ = sender.send(providers.clone());
            }
        }
    }

    /// Get statistics about the provider manager
    pub fn get_stats(&self) -> ProviderStats {
        let local_count = self.store.local_count();
        let (remote_manifests, total_remote_providers) = self.store.remote_stats();
        let pending_queries = self.store.pending_query_count();

        ProviderStats {
            local_providers: local_count,
            remote_manifests,
            total_remote_providers,
            pending_queries,
        }
    }
}

#[async_trait]
pub trait ProviderBackend: Send + Sync {
    fn announce_provider(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
        metadata: HashMap<String, String>,
    ) -> ProviderResult<()>;

    fn stop_providing(&self, manifest_id: &str) -> ProviderResult<()>;

    async fn discover_providers(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<Vec<ProviderInfo>>;

    fn get_local_providers(&self) -> Vec<ProviderInfo>;

    fn get_providers_for_manifest(&self, manifest_id: &str) -> Vec<ProviderInfo>;

    fn cleanup_expired_providers(&self);

    fn reannounce_local_providers(&self, swarm: &mut Swarm<MyBehaviour>);

    fn start_background_tasks(&self, swarm: &mut Swarm<MyBehaviour>);

    fn handle_provider_found(&self, manifest_id: &str, provider_peer: PeerId);

    fn get_stats(&self) -> ProviderStats;
}

#[async_trait]
impl ProviderBackend for ProviderManager {
    fn announce_provider(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
        metadata: HashMap<String, String>,
    ) -> ProviderResult<()> {
        ProviderManager::announce_provider(self, swarm, manifest_id, metadata)
    }

    fn stop_providing(&self, manifest_id: &str) -> ProviderResult<()> {
        ProviderManager::stop_providing(self, manifest_id)
    }

    async fn discover_providers(
        &self,
        swarm: &mut Swarm<MyBehaviour>,
        manifest_id: &str,
    ) -> ProviderResult<Vec<ProviderInfo>> {
        ProviderManager::discover_providers(self, swarm, manifest_id).await
    }

    fn get_local_providers(&self) -> Vec<ProviderInfo> {
        ProviderManager::get_local_providers(self)
    }

    fn get_providers_for_manifest(&self, manifest_id: &str) -> Vec<ProviderInfo> {
        ProviderManager::get_providers_for_manifest(self, manifest_id)
    }

    fn cleanup_expired_providers(&self) {
        ProviderManager::cleanup_expired_providers(self)
    }

    fn reannounce_local_providers(&self, swarm: &mut Swarm<MyBehaviour>) {
        ProviderManager::reannounce_local_providers(self, swarm)
    }

    fn start_background_tasks(&self, swarm: &mut Swarm<MyBehaviour>) {
        ProviderManager::start_background_tasks(self, swarm)
    }

    fn handle_provider_found(&self, manifest_id: &str, provider_peer: PeerId) {
        ProviderManager::handle_provider_found(self, manifest_id, provider_peer)
    }

    fn get_stats(&self) -> ProviderStats {
        ProviderManager::get_stats(self)
    }
}

/// Statistics about the provider manager
#[derive(Debug, Clone)]
pub struct ProviderStats {
    /// Number of manifests this node is providing
    pub local_providers: usize,
    /// Number of remote manifests we know providers for
    pub remote_manifests: usize,
    /// Total number of remote providers tracked
    pub total_remote_providers: usize,
    /// Number of pending discovery queries
    pub pending_queries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::PeerId;

    #[test]
    fn test_provider_info_expiration() {
        let mut provider = ProviderInfo {
            peer_id: PeerId::random(),
            manifest_id: "test".to_string(),
            announced_at: SystemTime::now() - Duration::from_secs(3700), // 1 hour 1 minute ago
            last_seen: SystemTime::now(),
            ttl_seconds: 3600, // 1 hour TTL
            metadata: HashMap::new(),
        };

        assert!(provider.is_expired());

        // Update to recent announcement
        provider.announced_at = SystemTime::now();
        assert!(!provider.is_expired());
    }

    #[test]
    fn test_provider_manager_creation() {
        let manager = ProviderManager::new_default();
        let stats = manager.get_stats();

        assert_eq!(stats.local_providers, 0);
        assert_eq!(stats.remote_manifests, 0);
        assert_eq!(stats.total_remote_providers, 0);
        assert_eq!(stats.pending_queries, 0);
    }

    #[test]
    fn test_local_provider_management() {
        let manager = ProviderManager::new_default();

        // Initially no providers
        assert!(manager.get_local_providers().is_empty());

        // Test stop providing non-existent manifest
        assert!(manager.stop_providing("non-existent").is_err());
    }

    #[test]
    fn test_provider_stats() {
        let manager = ProviderManager::new_default();
        let stats = manager.get_stats();

        assert_eq!(stats.local_providers, 0);
        assert_eq!(stats.remote_manifests, 0);
        assert_eq!(stats.total_remote_providers, 0);
    }

    #[test]
    fn test_cleanup_expired_providers() {
        let manager = ProviderManager::new_default();

        // Add an expired provider
        let expired_provider = ProviderInfo {
            peer_id: PeerId::random(),
            manifest_id: "test".to_string(),
            announced_at: SystemTime::now() - Duration::from_secs(7200), // 2 hours ago
            last_seen: SystemTime::now(),
            ttl_seconds: 3600, // 1 hour TTL
            metadata: HashMap::new(),
        };
        manager.test_insert_remote_provider("test", expired_provider);

        // Stats should show 1 remote provider
        assert_eq!(manager.get_stats().total_remote_providers, 1);

        // Cleanup should remove it
        manager.cleanup_expired_providers();
        assert_eq!(manager.get_stats().total_remote_providers, 0);
    }
}

#[cfg(test)]
impl ProviderManager {
    fn test_insert_remote_provider(&self, manifest_id: &str, provider: ProviderInfo) {
        self.store
            .set_remote(manifest_id.to_string(), vec![provider]);
    }
}
