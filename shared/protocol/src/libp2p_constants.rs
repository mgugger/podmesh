//! Consolidated libp2p-related constants for topics, message prefixes, protocol names, etc.
//!
//! This module contains all protocol constants to eliminate duplication across crates.

/// Ident topic used for workload-plane gossipsub exchanges.
pub const WORKLOAD_CLUSTER_TOPIC: &str = "podmesh-workload";

/// Ident topic used for machine-plane gossipsub exchanges.
pub const MACHINE_CLUSTER_TOPIC: &str = "podmesh-machine";

/// Record key prefix for manifest->sidecar announcements on the workload DHT.
pub const MANIFEST_RECORD_PREFIX: &str = "podmesh/manifest/";

/// Default manifest identifier used by the ingress e2e test.
pub const DEFAULT_INGRESS_MANIFEST_ID: &str = "demo-app";

/// Domain suffix appended to ingress hosts across the workload plane.
pub const MESH_DOMAIN_SUFFIX: &str = "mesh.local";

/// Protocol ID for the ingress proxy libp2p request-response stream.
pub const INGRESS_PROXY_PROTOCOL: &str = "/podmesh/ingress-proxy/1.0.0";
/// Protocol ID for sidecar manifest fetch RPCs between proxies and sidecars.
pub const SIDECAR_MANIFEST_PROTOCOL: &str = "/podmesh/sidecar-manifest/1.0.0";

// === GOSSIPSUB TOPICS ===

/// Protocol name for request-response RPCs (ApplyRequest/ApplyResponse).
pub const SCHEDULER_TASKS_TOPIC: &str = "scheduler-tasks";
/// Alias for backwards compatibility
pub const TOPIC_TASKS: &str = SCHEDULER_TASKS_TOPIC;

/// Topic used for scheduler events
pub const SCHEDULER_EVENTS_TOPIC: &str = "scheduler-events";
/// Alias for backwards compatibility
pub const TOPIC_EVENTS: &str = SCHEDULER_EVENTS_TOPIC;

/// Topic used for scheduler proposals / capacity requests
pub const SCHEDULER_PROPOSALS_TOPIC: &str = "scheduler-proposal";
/// Alias for backwards compatibility
pub const TOPIC_PROPOSALS: &str = "scheduler-proposals";

// === MESSAGE PREFIXES ===

/// Prefix used for handshake messages exchanged on the gossip topic.
pub const HANDSHAKE_PREFIX: &str = "podmesh-handshake";

/// Prefix used when querying peers for free capacity (gossipsub message topic payload prefix).
pub const FREE_CAPACITY_PREFIX: &str = "podmesh-free-capacity";

/// Prefix used for replies to free-capacity queries.
pub const FREE_CAPACITY_REPLY_PREFIX: &str = "podmesh-free-capacity-reply";

/// Prefix used for lease-related operations
pub const LEASE_PREFIX: &str = "lease/";

// === TIMEOUTS AND TIMING ===

/// Timeout, in milliseconds, to wait for free-capacity responses from peers.
/// This timeout should be long enough to handle slower network conditions (e.g., CI environments)
/// while still providing reasonable responsiveness for production use.
pub const FREE_CAPACITY_TIMEOUT_MS: u64 = 2000;

/// Timeout, in seconds, to wait for request-response RPCs (ApplyRequest/ApplyResponse)
pub const REQUEST_RESPONSE_TIMEOUT_SECS: u64 = 3;

/// Default selection window in milliseconds for scheduler operations
pub const DEFAULT_SELECTION_WINDOW_MS: u64 = 250;

/// Default lease TTL in milliseconds
pub const DEFAULT_LEASE_TTL_MS: u64 = 3000;

// === MANIFEST FIELDS ===

/// JSON field name used for replica count in manifests (top-level `replicas`).
pub const REPLICAS_FIELD: &str = "replicas";

/// JSON path field used for replica count in manifests under `spec.replicas`.
pub const SPEC_REPLICAS_FIELD: &str = "spec";

// === PROTOCOL VERSIONING ===

/// Version byte used in the compact binary envelope for capreq/capreply messages.
pub const BINARY_ENVELOPE_VERSION: u8 = 1;

// === RESOURCE MANAGEMENT CONSTANTS ===

/// Maximum percentage of CPU that can be allocated to workloads (90% to leave headroom)
pub const MAX_CPU_ALLOCATION_PERCENT: u8 = 90;

/// Maximum percentage of memory that can be allocated to workloads (90% to leave headroom)
pub const MAX_MEMORY_ALLOCATION_PERCENT: u8 = 90;

/// Maximum percentage of storage that can be allocated to workloads (90% to leave headroom)
pub const MAX_STORAGE_ALLOCATION_PERCENT: u8 = 90;

/// Minimum free memory to keep available in bytes (512 MB)
pub const MIN_FREE_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// Minimum free storage to keep available in bytes (1 GB)
pub const MIN_FREE_STORAGE_BYTES: u64 = 1024 * 1024 * 1024;

/// Maximum number of workloads per node (0 = unlimited)
pub const MAX_WORKLOADS_PER_NODE: u32 = 0;

/// Timeout for resource availability checks in milliseconds
pub const RESOURCE_CHECK_TIMEOUT_MS: u64 = 1000;

/// Default CPU request in millicores if not specified in manifest (100m = 0.1 core)
pub const DEFAULT_CPU_REQUEST_MILLI: u32 = 100;

/// Default memory request in bytes if not specified in manifest (128 MB)
pub const DEFAULT_MEMORY_REQUEST_BYTES: u64 = 128 * 1024 * 1024;

/// Default storage request in bytes if not specified in manifest (1 GB)
pub const DEFAULT_STORAGE_REQUEST_BYTES: u64 = 1024 * 1024 * 1024;

// === KADEMLIA CONFIGURATION CONSTANTS ===

/// Kademlia replication factor for provider records
pub const KADEMLIA_REPLICATION_FACTOR: usize = 3;

/// Maximum packet size for Kademlia messages (1 MB)
pub const KADEMLIA_MAX_PACKET_SIZE: usize = 1024 * 1024;

/// Parallelism factor for Kademlia queries
pub const KADEMLIA_PARALLELISM: usize = 3;

/// Query timeout for Kademlia operations in seconds
pub const KADEMLIA_QUERY_TIMEOUT_SECS: u64 = 15;

/// Provider record TTL in seconds
pub const KADEMLIA_PROVIDER_TTL_SECS: u64 = 30;

/// Provider publication interval in seconds
pub const KADEMLIA_PROVIDER_PUBLICATION_INTERVAL_SECS: u64 = 5;

// === GOSSIPSUB CONFIGURATION CONSTANTS ===

/// Heartbeat interval for gossipsub in seconds
pub const GOSSIPSUB_HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// Minimum mesh size for gossipsub
pub const GOSSIPSUB_MESH_N_LOW: usize = 1;

/// Target mesh size for gossipsub
pub const GOSSIPSUB_MESH_N: usize = 3;

/// Maximum mesh size for gossipsub
pub const GOSSIPSUB_MESH_N_HIGH: usize = 6;

/// Minimum outbound mesh size for gossipsub
pub const GOSSIPSUB_MESH_OUTBOUND_MIN: usize = 1;

// === CAPACITY REQUEST CONSTANTS ===

/// Default maximum hops for capacity requests before they stop being forwarded.
/// This limits amplification attacks while still allowing mesh-wide discovery.
pub const CAPACITY_REQUEST_DEFAULT_MAX_HOPS: u8 = 3;
