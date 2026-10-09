//! The proxy's routing table, and who is allowed to write to it.
//!
//! Routes are the proxy's most sensitive state: whoever owns the entry for a
//! hostname receives that hostname's traffic, headers and cookies included. The
//! table therefore records the owner key behind every entry and refuses a
//! registration that would move an entry, or a hostname, to a different owner.
//!
//! Every replica of a deployment registers the same routing key, so an entry
//! holds one backend per replica index and ingress rotates between them. A
//! table that kept only the last registration would hand the whole deployment's
//! traffic to whichever replica refreshed most recently, leaving the rest idle
//! and blackholing traffic whenever that one replica died.
//!
//! The routing key itself is derived from the owner key
//! (`protocol::route_id`), so claiming a particular key already requires
//! presenting the owner key it was derived from — and the caller must also hold
//! a live grant signed by that owner. Hostnames are free-form, so those are
//! first-claim-wins per owner and expire with the registration.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use protocol::{SidecarRegistration, SidecarRoute};

/// One replica serving a routing key.
#[derive(Debug, Clone)]
pub struct Backend {
    pub sidecar_peer_id: String,
    pub routes: Vec<SidecarRoute>,
    pub registered_at: u64,
}

/// Where ingress should send one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub replica_index: u32,
    pub sidecar_peer_id: String,
    pub port: u16,
}

/// Every replica registered under one routing key.
#[derive(Debug)]
pub struct SidecarRouteEntry {
    /// Base64 Ed25519 key of the namespace owner that registered this entry.
    pub owner_pubkey: String,
    pub workload_name: String,
    pub replica_count: u32,
    /// Backends by replica index. Keyed on the index rather than the sidecar's
    /// transport id so a restarted replica replaces its own entry instead of
    /// appearing alongside the dead one until that timed out.
    pub backends: BTreeMap<u32, Backend>,
    /// Rotates the starting point so consecutive requests spread across
    /// replicas instead of always beginning at the lowest index.
    cursor: AtomicUsize,
}

#[derive(Debug, Clone)]
struct HostClaim {
    manifest_id: String,
    owner_pubkey: String,
    registered_at: u64,
}

#[derive(Default)]
struct Inner {
    by_manifest: HashMap<String, SidecarRouteEntry>,
    host_claims: HashMap<String, HostClaim>,
}

/// Shared routing state.
#[derive(Default)]
pub struct RouteTable {
    inner: RwLock<Inner>,
    metrics: podmesh_metrics::Metrics,
}

impl RouteTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_metrics(mut self, metrics: podmesh_metrics::Metrics) -> Self {
        metrics.set_gauge(podmesh_metrics::GaugeName::RouteKeys, 0);
        metrics.set_gauge(podmesh_metrics::GaugeName::RouteBackends, 0);
        self.metrics = metrics;
        self
    }

    /// Install or refresh a registration.
    ///
    /// Returns an error rather than overwriting when the entry, or one of the
    /// hostnames it claims, already belongs to a different owner. Silently
    /// taking over would be a cross-tenant hijack.
    pub fn register(
        &self,
        registration: &SidecarRegistration,
        registered_at: u64,
        max_entries: usize,
    ) -> Result<()> {
        let timer = self
            .metrics
            .operation_started(podmesh_metrics::OperationName::RouteUpdate);
        let result = self.register_inner(registration, registered_at, max_entries);
        self.refresh_metrics();
        match &result {
            Ok(()) => timer.finish(
                podmesh_metrics::Outcome::Success,
                podmesh_metrics::Reason::None,
            ),
            Err(error) if error.to_string().contains("capacity") => {
                self.metrics
                    .record_event(podmesh_metrics::EventName::StoreSaturation);
                timer.finish(
                    podmesh_metrics::Outcome::Saturated,
                    podmesh_metrics::Reason::Capacity,
                );
            }
            Err(_) => timer.finish(
                podmesh_metrics::Outcome::Refused,
                podmesh_metrics::Reason::Policy,
            ),
        }
        result
    }

    fn register_inner(
        &self,
        registration: &SidecarRegistration,
        registered_at: u64,
        max_entries: usize,
    ) -> Result<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| anyhow!("routing table lock poisoned"))?;

        if let Some(existing) = inner.by_manifest.get(&registration.manifest_id)
            && existing.owner_pubkey != registration.owner_pubkey
        {
            bail!(
                "manifest {} belongs to another owner",
                registration.manifest_id
            );
        }
        if !inner.by_manifest.contains_key(&registration.manifest_id)
            && inner.by_manifest.len() >= max_entries
        {
            bail!("proxy sidecar route capacity reached");
        }

        // Check every hostname before mutating, so a rejected registration
        // leaves no partial claim behind.
        let hosts: Vec<&str> = registration
            .routes
            .iter()
            .filter_map(SidecarRoute::host)
            .collect();
        for host in &hosts {
            let host = host.to_ascii_lowercase();
            if let Some(claim) = inner.host_claims.get(&host)
                && claim.owner_pubkey != registration.owner_pubkey
            {
                bail!("host {host} is already claimed by another owner");
            }
        }

        for host in hosts {
            inner.host_claims.insert(
                host.to_ascii_lowercase(),
                HostClaim {
                    manifest_id: registration.manifest_id.clone(),
                    owner_pubkey: registration.owner_pubkey.clone(),
                    registered_at,
                },
            );
        }
        // Refresh this replica's backend, leaving its siblings in place.
        let entry = inner
            .by_manifest
            .entry(registration.manifest_id.clone())
            .or_insert_with(|| SidecarRouteEntry {
                owner_pubkey: registration.owner_pubkey.clone(),
                workload_name: registration.workload_name.clone(),
                replica_count: registration.replica_count,
                backends: BTreeMap::new(),
                cursor: AtomicUsize::new(0),
            });
        entry.replica_count = registration.replica_count;
        entry.backends.insert(
            registration.replica_index,
            Backend {
                sidecar_peer_id: registration.sidecar_peer_id.clone(),
                routes: registration.routes.clone(),
                registered_at,
            },
        );
        Ok(())
    }

    /// Every backend that can serve `path` for `host`, most preferred first.
    ///
    /// The list is rotated on each call, so consecutive requests land on
    /// different replicas. Returning the whole list rather than one target lets
    /// the caller fall through to the next replica when one is unreachable,
    /// which is what keeps a dead replica from blackholing traffic until its
    /// registration expires.
    pub fn select(&self, manifest_id: &str, path: &str, host: Option<&str>) -> Vec<RouteTarget> {
        let Ok(inner) = self.inner.read() else {
            log::error!("routing table lock poisoned; serving no route");
            return Vec::new();
        };
        let Some(entry) = inner.by_manifest.get(manifest_id) else {
            return Vec::new();
        };
        let candidates: Vec<RouteTarget> = entry
            .backends
            .iter()
            .filter_map(|(replica_index, backend)| {
                select_route_port(&backend.routes, path, host).map(|port| RouteTarget {
                    replica_index: *replica_index,
                    sidecar_peer_id: backend.sidecar_peer_id.clone(),
                    port,
                })
            })
            .collect();
        if candidates.is_empty() {
            return candidates;
        }
        let start = entry.cursor.fetch_add(1, Ordering::Relaxed) % candidates.len();
        candidates[start..]
            .iter()
            .chain(candidates[..start].iter())
            .cloned()
            .collect()
    }

    /// How many replicas currently serve a routing key.
    pub fn backend_count(&self, manifest_id: &str) -> usize {
        self.inner
            .read()
            .ok()
            .and_then(|inner| inner.by_manifest.get(manifest_id).map(|e| e.backends.len()))
            .unwrap_or(0)
    }

    pub fn contains(&self, manifest_id: &str) -> bool {
        self.backend_count(manifest_id) > 0
    }

    /// Resolve an ingress hostname to the routing key that claimed it.
    pub fn resolve_host(&self, host: &str) -> Option<String> {
        self.inner
            .read()
            .ok()?
            .host_claims
            .get(&host.to_ascii_lowercase())
            .map(|claim| claim.manifest_id.clone())
    }

    /// Drop backends, entries and host claims that stopped refreshing.
    ///
    /// Pruning is per replica: one dead replica is removed while its siblings
    /// keep serving, and the entry only disappears once every replica is gone.
    pub fn prune(&self, now_millis: u64, ttl: Duration) {
        let timer = self
            .metrics
            .operation_started(podmesh_metrics::OperationName::RouteUpdate);
        self.prune_inner(now_millis, ttl);
        self.refresh_metrics();
        timer.finish(
            podmesh_metrics::Outcome::Success,
            podmesh_metrics::Reason::None,
        );
    }

    fn prune_inner(&self, now_millis: u64, ttl: Duration) {
        let Ok(mut inner) = self.inner.write() else {
            log::error!("routing table lock poisoned; skipping prune");
            return;
        };
        let ttl_millis = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        let stale = |registered_at: u64| now_millis.saturating_sub(registered_at) > ttl_millis;
        for entry in inner.by_manifest.values_mut() {
            entry
                .backends
                .retain(|_, backend| !stale(backend.registered_at));
        }
        inner
            .by_manifest
            .retain(|_, entry| !entry.backends.is_empty());
        inner
            .host_claims
            .retain(|_, claim| !stale(claim.registered_at));
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .map(|inner| inner.by_manifest.len())
            .unwrap_or(0)
    }

    pub fn total_backends(&self) -> usize {
        self.inner
            .read()
            .map(|inner| {
                inner
                    .by_manifest
                    .values()
                    .map(|entry| entry.backends.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    fn refresh_metrics(&self) {
        let (route_keys, route_backends) = self
            .inner
            .read()
            .map(|inner| {
                (
                    inner.by_manifest.len(),
                    inner
                        .by_manifest
                        .values()
                        .map(|entry| entry.backends.len())
                        .sum::<usize>(),
                )
            })
            .unwrap_or_default();
        self.metrics
            .set_gauge(podmesh_metrics::GaugeName::RouteKeys, route_keys as u64);
        self.metrics.set_gauge(
            podmesh_metrics::GaugeName::RouteBackends,
            route_backends as u64,
        );
    }

    /// Routing keys with at least one live backend, for reporting.
    pub fn summary(&self) -> Vec<(String, String, usize, u32)> {
        let Ok(inner) = self.inner.read() else {
            return Vec::new();
        };
        inner
            .by_manifest
            .iter()
            .map(|(manifest_id, entry)| {
                (
                    manifest_id.clone(),
                    entry.workload_name.clone(),
                    entry.backends.len(),
                    entry.replica_count,
                )
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Pick the port serving `path` for `host`.
///
/// Host-scoped routes win over host-agnostic ones, and among equally specific
/// matches the longest path prefix wins. Ties are broken by port so the choice
/// is deterministic rather than dependent on map iteration order.
pub(crate) fn select_route_port(
    routes: &[SidecarRoute],
    path: &str,
    host: Option<&str>,
) -> Option<u16> {
    let normalized = path.split('?').next().unwrap_or(path);
    routes
        .iter()
        .filter(|route| normalized.starts_with(&route.path_prefix))
        .filter(|route| match (route.host(), host) {
            (None, _) => true,
            (Some(route_host), Some(request_host)) => route_host.eq_ignore_ascii_case(request_host),
            (Some(_), None) => false,
        })
        .max_by_key(|route| {
            (
                route.host().is_some(),
                route.path_prefix.len(),
                std::cmp::Reverse(route.port),
            )
        })
        .map(|route| route.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_ENTRIES: usize = 8;

    fn registration(
        owner: u8,
        name: &str,
        replica_index: u32,
        replica_count: u32,
        host: &str,
        peer: &str,
    ) -> SidecarRegistration {
        SidecarRegistration::new(
            &[owner; 32],
            name,
            replica_index,
            replica_count,
            vec![SidecarRoute {
                host: host.into(),
                path_prefix: "/".into(),
                port: 8080,
            }],
            peer,
        )
    }

    fn single(owner: u8, name: &str, host: &str, peer: &str) -> SidecarRegistration {
        registration(owner, name, 0, 1, host, peer)
    }

    #[test]
    fn an_owner_can_refresh_its_own_entry() {
        let metrics = podmesh_metrics::Metrics::registered(podmesh_metrics::ComponentName::Proxy);
        let table = RouteTable::new().with_metrics(metrics.clone());
        let first = single(1, "demo", "demo.example", "peer-a");
        table.register(&first, 0, MAX_ENTRIES).unwrap();
        table.register(&first, 10, MAX_ENTRIES).unwrap();
        assert_eq!(table.backend_count(&first.manifest_id), 1);
        let snapshot = metrics.snapshot().unwrap();
        assert!(snapshot.gauges().any(|(key, value)| {
            key.gauge() == podmesh_metrics::GaugeName::RouteKeys && *value == 1
        }));
        assert!(snapshot.gauges().any(|(key, value)| {
            key.gauge() == podmesh_metrics::GaugeName::RouteBackends && *value == 1
        }));
        assert!(snapshot.operations().any(|(key, value)| {
            key.operation() == podmesh_metrics::OperationName::RouteUpdate && value.count() == 2
        }));
    }

    /// Every replica registers the same routing key. The table must hold one
    /// backend per replica, or all but the last registrant are dropped.
    #[test]
    fn every_replica_becomes_its_own_backend() {
        let table = RouteTable::new();
        for replica in 0..3u32 {
            let entry = registration(
                1,
                "demo",
                replica,
                3,
                "demo.example",
                &format!("peer-{replica}"),
            );
            table.register(&entry, 0, MAX_ENTRIES).unwrap();
        }
        let manifest_id = protocol::route_id(&[1u8; 32], "demo");
        assert_eq!(table.backend_count(&manifest_id), 3);
        assert_eq!(
            table.select(&manifest_id, "/", Some("demo.example")).len(),
            3
        );
    }

    /// A replica that restarts comes back with a fresh transport id. Keying on
    /// the replica index is what stops it from appearing beside its own corpse.
    #[test]
    fn a_restarted_replica_replaces_its_own_backend() {
        let table = RouteTable::new();
        table
            .register(
                &registration(1, "demo", 0, 2, "demo.example", "peer-old"),
                0,
                MAX_ENTRIES,
            )
            .unwrap();
        table
            .register(
                &registration(1, "demo", 1, 2, "demo.example", "peer-b"),
                0,
                MAX_ENTRIES,
            )
            .unwrap();
        table
            .register(
                &registration(1, "demo", 0, 2, "demo.example", "peer-new"),
                5,
                MAX_ENTRIES,
            )
            .unwrap();

        let manifest_id = protocol::route_id(&[1u8; 32], "demo");
        assert_eq!(table.backend_count(&manifest_id), 2);
        let peers: Vec<String> = table
            .select(&manifest_id, "/", Some("demo.example"))
            .into_iter()
            .map(|target| target.sidecar_peer_id)
            .collect();
        assert!(peers.contains(&"peer-new".to_string()));
        assert!(!peers.contains(&"peer-old".to_string()));
    }

    /// Consecutive requests must not all land on the same replica, or the
    /// deployment has N times the cost for 1x the throughput.
    #[test]
    fn consecutive_requests_rotate_across_replicas() {
        let table = RouteTable::new();
        for replica in 0..3u32 {
            let entry = registration(
                1,
                "demo",
                replica,
                3,
                "demo.example",
                &format!("peer-{replica}"),
            );
            table.register(&entry, 0, MAX_ENTRIES).unwrap();
        }
        let manifest_id = protocol::route_id(&[1u8; 32], "demo");

        let firsts: Vec<u32> = (0..3)
            .map(|_| table.select(&manifest_id, "/", Some("demo.example"))[0].replica_index)
            .collect();
        assert_eq!(
            firsts,
            vec![0, 1, 2],
            "each request must start at the next replica"
        );

        // Every call still offers every replica, so a caller can fall through
        // when one is unreachable.
        for _ in 0..3 {
            let mut seen: Vec<u32> = table
                .select(&manifest_id, "/", Some("demo.example"))
                .into_iter()
                .map(|target| target.replica_index)
                .collect();
            seen.sort_unstable();
            assert_eq!(seen, vec![0, 1, 2]);
        }
    }

    #[test]
    fn another_owner_cannot_take_over_a_hostname() {
        let table = RouteTable::new();
        let victim = single(1, "demo", "demo.example", "peer-a");
        table.register(&victim, 0, MAX_ENTRIES).unwrap();

        let attacker = single(2, "evil", "demo.example", "peer-b");
        let error = table.register(&attacker, 1, MAX_ENTRIES).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already claimed by another owner")
        );

        assert_eq!(
            table.resolve_host("demo.example").unwrap(),
            victim.manifest_id
        );
    }

    #[test]
    fn a_rejected_registration_claims_nothing() {
        let table = RouteTable::new();
        table
            .register(
                &single(1, "demo", "taken.example", "peer-a"),
                0,
                MAX_ENTRIES,
            )
            .unwrap();

        let mut attacker = single(2, "evil", "free.example", "peer-b");
        attacker.routes.push(SidecarRoute {
            host: "taken.example".into(),
            path_prefix: "/".into(),
            port: 9090,
        });
        assert!(table.register(&attacker, 1, MAX_ENTRIES).is_err());
        assert!(table.resolve_host("free.example").is_none());
    }

    #[test]
    fn hostnames_resolve_case_insensitively() {
        let table = RouteTable::new();
        let entry = single(1, "demo", "Demo.Example", "peer-a");
        table.register(&entry, 0, MAX_ENTRIES).unwrap();
        assert_eq!(
            table.resolve_host("demo.EXAMPLE").unwrap(),
            entry.manifest_id
        );
    }

    /// One dead replica must be pruned without taking its siblings with it.
    #[test]
    fn pruning_removes_only_the_replicas_that_stopped_refreshing() {
        let table = RouteTable::new();
        table
            .register(
                &registration(1, "demo", 0, 2, "demo.example", "peer-a"),
                0,
                MAX_ENTRIES,
            )
            .unwrap();
        table
            .register(
                &registration(1, "demo", 1, 2, "demo.example", "peer-b"),
                9_000,
                MAX_ENTRIES,
            )
            .unwrap();

        let manifest_id = protocol::route_id(&[1u8; 32], "demo");
        table.prune(10_000, Duration::from_secs(5));
        assert_eq!(
            table.backend_count(&manifest_id),
            1,
            "the replica that kept refreshing must keep serving"
        );

        table.prune(20_000, Duration::from_secs(5));
        assert!(!table.contains(&manifest_id));
        assert!(table.resolve_host("demo.example").is_none());
    }

    #[test]
    fn the_table_is_bounded() {
        let table = RouteTable::new();
        for index in 0..MAX_ENTRIES {
            let entry = single(1, &format!("demo-{index}"), "", "peer-a");
            table.register(&entry, 0, MAX_ENTRIES).unwrap();
        }
        let overflow = single(1, "one-too-many", "", "peer-a");
        assert!(table.register(&overflow, 0, MAX_ENTRIES).is_err());
    }
}
