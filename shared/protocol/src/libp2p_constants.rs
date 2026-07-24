pub const WORKLOAD_CLUSTER_TOPIC: &str = "podmesh-workload";
pub const MANIFEST_RECORD_PREFIX: &str = "podmesh/manifest/";
pub const MESH_DOMAIN_SUFFIX: &str = "mesh.local";

pub const INGRESS_PROXY_PROTOCOL: &str = "/podmesh/ingress-proxy/1.0.0";
pub const SIDECAR_MANIFEST_PROTOCOL: &str = "/podmesh/sidecar-manifest/1.0.0";
pub const EGRESS_TUNNEL_PROTOCOL: &str = "/podmesh/egress-tunnel/1.0.0";
pub const SIDECAR_REGISTRATION_PROTOCOL: &str = "/podmesh/sidecar-registration/1.0.0";
pub const PROXY_PROVIDER_KEY: &str = "podmesh-proxy-node";

pub const KADEMLIA_REPLICATION_FACTOR: usize = 3;
pub const KADEMLIA_MAX_PACKET_SIZE: usize = 1024 * 1024;
pub const KADEMLIA_PARALLELISM: usize = 3;
pub const KADEMLIA_QUERY_TIMEOUT_SECS: u64 = 15;
pub const KADEMLIA_PROVIDER_TTL_SECS: u64 = 30;
pub const KADEMLIA_PROVIDER_PUBLICATION_INTERVAL_SECS: u64 = 5;

pub const GOSSIPSUB_HEARTBEAT_INTERVAL_SECS: u64 = 10;
pub const GOSSIPSUB_MESH_N_LOW: usize = 1;
pub const GOSSIPSUB_MESH_N: usize = 3;
pub const GOSSIPSUB_MESH_N_HIGH: usize = 6;
pub const GOSSIPSUB_MESH_OUTBOUND_MIN: usize = 1;

pub const MANIFEST_RECORD_TTL_MS: u32 = 300_000;
pub const MANIFEST_CACHE_TTL_RATIO: f64 = 0.8;
