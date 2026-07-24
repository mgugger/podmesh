use clap::{Parser, ValueEnum};
use std::path::PathBuf;

pub const DEFAULT_CPU_MILLI: u32 = 4_000;
pub const DEFAULT_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const DEFAULT_STORAGE_BYTES: u64 = 100 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_WORKLOADS: usize = 100;
pub const DEFAULT_WORKLOAD_NETWORK: &str = "podmesh";

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RuntimeKind {
    Podman,
    Mock,
}

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Config {
    #[arg(long, default_value = "0.0.0.0:3100")]
    pub listen: String,

    #[arg(long, default_value = "http://127.0.0.1:3100")]
    pub advertise_url: String,

    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub scheduler_url: String,

    #[arg(long, default_value = "/etc/podmesh/agent")]
    pub key_dir: PathBuf,

    #[arg(long, default_value = "/var/lib/podmesh-agent/state.redb")]
    pub state_path: PathBuf,

    #[arg(long, value_enum, default_value_t = RuntimeKind::Podman)]
    pub runtime: RuntimeKind,

    #[arg(long, default_value = DEFAULT_WORKLOAD_NETWORK)]
    pub workload_network: String,

    #[arg(long, default_value = "podmesh/sidecar:latest")]
    pub sidecar_image: String,

    #[arg(long, default_value = protocol::sidecar_metadata::DEFAULT_SIDECAR_BOOTSTRAP_MULTIADDR)]
    pub sidecar_bootstrap_peer: String,

    #[arg(long, default_value_t = DEFAULT_CPU_MILLI)]
    pub capacity_cpu_milli: u32,

    #[arg(long, default_value_t = DEFAULT_MEMORY_BYTES)]
    pub capacity_memory_bytes: u64,

    #[arg(long, default_value_t = DEFAULT_STORAGE_BYTES)]
    pub capacity_storage_bytes: u64,

    #[arg(long, default_value_t = DEFAULT_MAX_WORKLOADS)]
    pub max_workloads: usize,
}
