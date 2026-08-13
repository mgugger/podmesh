use clap::{Parser, ValueEnum};
use std::path::PathBuf;

pub const DEFAULT_CPU_MILLI: u32 = 4_000;
pub const DEFAULT_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const DEFAULT_STORAGE_BYTES: u64 = 100 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_WORKLOADS: usize = 100;
pub const DEFAULT_WORKLOAD_NETWORK: &str = "podmesh";
/// Share of capacity that admissions which have not deployed may hold.
///
/// A reservation is one signed message and is not checked against a real
/// workload until deploy, so leaving it uncapped would let unfinished
/// admissions drive advertised capacity to zero. Half leaves room for real
/// placements while still allowing a burst of concurrent deployments.
pub const DEFAULT_MAX_RESERVED_CAPACITY_PERCENT: u32 = 50;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RuntimeKind {
    Podman,
    Mock,
}

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Config {
    /// Liveness-only HTTP listener. The agent's control plane is reachable
    /// exclusively over Iroh, so nothing else is served here.
    #[arg(long, default_value = "0.0.0.0:3100")]
    pub listen: String,

    #[arg(long, default_value = "/etc/podmesh/agent")]
    pub key_dir: PathBuf,

    #[arg(long, default_value = "/var/lib/podmesh-agent/state.redb")]
    pub state_path: PathBuf,

    #[arg(long, value_enum, default_value_t = RuntimeKind::Podman)]
    pub runtime: RuntimeKind,

    /// Network for workload pods.
    ///
    /// Every tenant's pods share it, so this is a transport, not a boundary:
    /// tenants are separated by owner identity at the proxy, not by topology.
    #[arg(long, default_value = DEFAULT_WORKLOAD_NETWORK)]
    pub workload_network: String,

    /// Share of capacity that pending reservations may hold, as a percentage.
    ///
    /// Running workloads may use the whole agent; this bounds only admissions
    /// that have not deployed yet, so an unfinished admission cannot zero out
    /// the capacity this agent advertises. 100 disables the bound.
    #[arg(long, default_value_t = DEFAULT_MAX_RESERVED_CAPACITY_PERCENT)]
    pub max_reserved_capacity_percent: u32,

    #[arg(long, default_value = "podmesh/sidecar:latest")]
    pub sidecar_image: String,

    #[arg(long, default_value_t = DEFAULT_CPU_MILLI)]
    pub capacity_cpu_milli: u32,

    #[arg(long, default_value_t = DEFAULT_MEMORY_BYTES)]
    pub capacity_memory_bytes: u64,

    #[arg(long, default_value_t = DEFAULT_STORAGE_BYTES)]
    pub capacity_storage_bytes: u64,

    #[arg(long, default_value_t = DEFAULT_MAX_WORKLOADS)]
    pub max_workloads: usize,

    #[command(flatten)]
    pub machine: crate::machine::MachineConfig,
}
