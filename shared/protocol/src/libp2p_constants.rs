pub const WORKLOAD_CLUSTER_TOPIC: &str = "podmesh-workload";
pub const MESH_DOMAIN_SUFFIX: &str = "mesh.local";

pub const INGRESS_PROXY_PROTOCOL: &str = "/podmesh/ingress-proxy/1.0.0";
pub const EGRESS_TUNNEL_PROTOCOL: &str = "/podmesh/egress-tunnel/1.0.0";
pub const SIDECAR_REGISTRATION_PROTOCOL: &str = "/podmesh/sidecar-registration/1.0.0";
pub const PROXY_DISCOVERY_PROTOCOL: &str = "/podmesh/proxy-discovery/1.0.0";

pub const GOSSIPSUB_HEARTBEAT_INTERVAL_SECS: u64 = 10;
pub const GOSSIPSUB_MESH_N_LOW: usize = 1;
pub const GOSSIPSUB_MESH_N: usize = 3;
pub const GOSSIPSUB_MESH_N_HIGH: usize = 6;
pub const GOSSIPSUB_MESH_OUTBOUND_MIN: usize = 1;
