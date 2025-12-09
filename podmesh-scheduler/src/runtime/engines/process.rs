//! Process-based runtime engine for testing
//!
//! This module provides a runtime engine that spawns actual OS processes
//! instead of containers. It's useful for integration testing where you want
//! to test the full system behavior without depending on container runtimes.
//!
//! The engine spawns the sidecar binary directly as a child process,
//! allowing testing of the complete scheduling and workload lifecycle
//! without Docker or Podman.

use crate::runtime::{
    DeploymentConfig, RuntimeEngine, RuntimeError, RuntimeResult,
    WorkloadInfo, WorkloadStatus,
};
use crate::sidecar;
use async_trait::async_trait;
use log::{debug, info, warn};
use protocol::manifest_yaml::parse_yaml_documents_from_str;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

/// Maximum number of concurrent process workloads.
const MAX_PROCESS_WORKLOADS: usize = 100;

/// A deployed workload as a native process.
#[derive(Debug)]
pub struct ProcessWorkload {
    pub info: WorkloadInfo,
    pub config: DeploymentConfig,
    /// Child process handle (if running).
    pub process: Option<Child>,
    /// PID of the running process.
    pub pid: Option<u32>,
}

/// Configuration for the process engine.
#[derive(Debug, Clone)]
pub struct ProcessEngineConfig {
    /// Path to the sidecar binary. If None, will attempt to find it in target/debug or target/release.
    pub sidecar_binary_path: Option<PathBuf>,
    /// Working directory for spawned processes.
    pub working_directory: Option<PathBuf>,
    /// Whether to capture stdout/stderr.
    pub capture_output: bool,
    /// Environment variables to pass to all spawned processes.
    pub env_vars: HashMap<String, String>,
    /// Base port for allocating ports to workloads (processes will use sequential ports).
    pub base_port: u16,
}

impl Default for ProcessEngineConfig {
    fn default() -> Self {
        Self {
            sidecar_binary_path: None,
            working_directory: None,
            capture_output: true,
            env_vars: HashMap::new(),
            base_port: 18100,
        }
    }
}

/// Process-based runtime engine that spawns OS processes for workloads.
pub struct ProcessEngine {
    /// In-memory storage of deployed workloads.
    workloads: Arc<RwLock<HashMap<String, ProcessWorkload>>>,
    /// Configuration for the engine.
    config: ProcessEngineConfig,
    /// Counter for generating unique workload IDs.
    workload_counter: Arc<Mutex<u64>>,
    /// Next available port for workloads.
    next_port: Arc<Mutex<u16>>,
}

impl ProcessEngine {
    /// Create a new process engine with default configuration.
    pub fn new() -> Self {
        Self {
            workloads: Arc::new(RwLock::new(HashMap::new())),
            config: ProcessEngineConfig::default(),
            workload_counter: Arc::new(Mutex::new(0)),
            next_port: Arc::new(Mutex::new(ProcessEngineConfig::default().base_port)),
        }
    }

    /// Create a new process engine with custom configuration.
    pub fn with_config(config: ProcessEngineConfig) -> Self {
        let base_port = config.base_port;
        Self {
            workloads: Arc::new(RwLock::new(HashMap::new())),
            config,
            workload_counter: Arc::new(Mutex::new(0)),
            next_port: Arc::new(Mutex::new(base_port)),
        }
    }

    /// Get the number of deployed workloads (for testing).
    pub async fn workload_count(&self) -> usize {
        self.workloads.read().await.len()
    }

    /// Clear all workloads, terminating any running processes.
    pub async fn clear_workloads(&self) {
        let mut workloads = self.workloads.write().await;
        for (id, mut workload) in workloads.drain() {
            if let Some(mut process) = workload.process.take() {
                info!("Terminating process workload {}", id);
                let _ = process.kill().await;
            }
        }
    }

    /// Get the deployment config for a workload (for testing verification).
    pub async fn get_workload_config(&self, workload_id: &str) -> Option<DeploymentConfig> {
        self.workloads
            .read()
            .await
            .get(workload_id)
            .map(|w| w.config.clone())
    }

    /// Allocate a unique port for a new workload.
    fn allocate_port(&self) -> u16 {
        let mut port = self.next_port.lock().unwrap();
        let allocated = *port;
        *port = port.wrapping_add(1);
        if *port < self.config.base_port {
            *port = self.config.base_port;
        }
        allocated
    }

    /// Generate a unique workload ID.
    fn generate_workload_id(&self, manifest_id: &str) -> String {
        let mut counter = self.workload_counter.lock().unwrap();
        *counter += 1;
        format!("process-{}-{}", manifest_id, *counter)
    }

    /// Find the sidecar binary path.
    fn find_sidecar_binary(&self) -> RuntimeResult<PathBuf> {
        if let Some(ref path) = self.config.sidecar_binary_path {
            if path.exists() {
                return Ok(path.clone());
            }
            return Err(RuntimeError::EngineNotAvailable(format!(
                "Configured sidecar binary not found at {:?}",
                path
            )));
        }

        // Try to find the binary in common locations
        let candidates = [
            PathBuf::from("target/debug/podmesh-sidecar"),
            PathBuf::from("target/release/podmesh-sidecar"),
            PathBuf::from("../target/debug/podmesh-sidecar"),
            PathBuf::from("../target/release/podmesh-sidecar"),
        ];

        for candidate in &candidates {
            if candidate.exists() {
                info!("Found sidecar binary at {:?}", candidate);
                return Ok(candidate.clone());
            }
        }

        // Try to find it relative to the current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(parent) = exe_path.parent() {
                let debug_path = parent.join("podmesh-sidecar");
                if debug_path.exists() {
                    info!("Found sidecar binary at {:?}", debug_path);
                    return Ok(debug_path);
                }
            }
        }

        Err(RuntimeError::EngineNotAvailable(
            "Could not find podmesh-sidecar binary. Build it with 'cargo build -p podmesh-sidecar'"
                .to_string(),
        ))
    }

    /// Parse manifest content to extract metadata.
    fn parse_manifest_metadata(&self, manifest_content: &[u8]) -> HashMap<String, String> {
        let manifest_str = String::from_utf8_lossy(manifest_content);
        let mut metadata = HashMap::new();

        // Try to parse as JSON first
        if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&manifest_str) {
            if let Some(kind) = json_val.get("kind").and_then(|v| v.as_str()) {
                metadata.insert("kind".to_string(), kind.to_string());
            }
            if let Some(api_version) = json_val.get("apiVersion").and_then(|v| v.as_str()) {
                metadata.insert("apiVersion".to_string(), api_version.to_string());
            }
            if let Some(meta_obj) = json_val.get("metadata").and_then(|v| v.as_object()) {
                if let Some(name) = meta_obj.get("name").and_then(|v| v.as_str()) {
                    metadata.insert("name".to_string(), name.to_string());
                }
                if let Some(namespace) = meta_obj.get("namespace").and_then(|v| v.as_str()) {
                    metadata.insert("namespace".to_string(), namespace.to_string());
                }
            }
        } else {
            // Fallback to YAML parsing
            for line in manifest_str.lines() {
                let line = line.trim();
                if line.starts_with("kind:") {
                    if let Some(value) = line.strip_prefix("kind:").map(|s| s.trim()) {
                        metadata.insert("kind".to_string(), value.to_string());
                    }
                } else if line.starts_with("apiVersion:") {
                    if let Some(value) = line.strip_prefix("apiVersion:").map(|s| s.trim()) {
                        metadata.insert("apiVersion".to_string(), value.to_string());
                    }
                } else if line.contains("name:") {
                    if let Some(name_part) = line.split("name:").nth(1) {
                        let value = name_part.trim();
                        if !value.is_empty() && !metadata.contains_key("name") {
                            metadata.insert("name".to_string(), value.to_string());
                        }
                    }
                }
            }
        }

        metadata
    }

    /// Spawn a sidecar process for the given manifest.
    async fn spawn_sidecar_process(
        &self,
        workload_id: &str,
        manifest_id: &str,
        manifest_content: &[u8],
        config: &DeploymentConfig,
    ) -> RuntimeResult<Child> {
        let sidecar_binary = self.find_sidecar_binary()?;
        // Allocate a port for potential future use (reserved for workload)
        let _port = self.allocate_port();

        // Build the sidecar metadata blob if sidecar injection config is available
        let metadata_blob = if let Some(ref sidecar_config) = config.sidecar {
            Some(
                sidecar::build_inline_metadata_blob(
                    manifest_id,
                    &sidecar_config.manifest_bytes,
                    &sidecar_config.owner_public_key,
                    &sidecar_config.bootstrap_peer,
                )
                .map_err(|e| RuntimeError::DeploymentFailed(format!("Failed to build metadata blob: {}", e)))?,
            )
        } else {
            // Build a minimal metadata blob from manifest content
            Some(
                sidecar::build_inline_metadata_blob(
                    manifest_id,
                    manifest_content,
                    &[],
                    &sidecar::DEFAULT_SIDECAR_BOOTSTRAP_MULTIADDR,
                )
                .map_err(|e| RuntimeError::DeploymentFailed(format!("Failed to build metadata blob: {}", e)))?,
            )
        };

        let mut cmd = Command::new(&sidecar_binary);

        // Set working directory if configured
        if let Some(ref working_dir) = self.config.working_directory {
            cmd.current_dir(working_dir);
        }

        // Set common arguments
        cmd.arg("--libp2p-port")
            .arg("0") // Use auto-assigned port
            .arg("--libp2p-host")
            .arg("127.0.0.1");

        // Pass the metadata blob via environment variable
        if let Some(ref blob) = metadata_blob {
            cmd.env("PODMESH_SIDECAR_METADATA_B64", blob);
        }

        // Set log level
        cmd.env("RUST_LOG", "info,libp2p=warn,quinn=warn");

        // Add custom environment variables
        for (key, value) in &self.config.env_vars {
            cmd.env(key, value);
        }

        // Add deployment config environment variables
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        // Configure stdio
        if self.config.capture_output {
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
        } else {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }

        cmd.stdin(Stdio::null());
        cmd.kill_on_drop(true);

        info!(
            "Spawning sidecar process for workload {} (manifest: {}) using binary {:?}",
            workload_id, manifest_id, sidecar_binary
        );

        let child = cmd.spawn().map_err(|e| {
            RuntimeError::DeploymentFailed(format!("Failed to spawn sidecar process: {}", e))
        })?;

        info!(
            "Sidecar process spawned with PID {:?} for workload {}",
            child.id(),
            workload_id
        );

        Ok(child)
    }

    /// Check if a process is still running.
    async fn is_process_running(child: &mut Child) -> bool {
        match child.try_wait() {
            Ok(Some(_)) => false, // Process has exited
            Ok(None) => true,     // Process is still running
            Err(_) => false,      // Error checking, assume not running
        }
    }
}

impl Default for ProcessEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RuntimeEngine for ProcessEngine {
    fn name(&self) -> &str {
        "process"
    }

    async fn is_available(&self) -> bool {
        self.find_sidecar_binary().is_ok()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn validate_manifest(&self, manifest_content: &[u8]) -> RuntimeResult<()> {
        let manifest_str = String::from_utf8_lossy(manifest_content);

        if manifest_str.trim().is_empty() {
            return Err(RuntimeError::InvalidManifest("Empty manifest".to_string()));
        }

        if parse_yaml_documents_from_str(&manifest_str)
            .map(|docs| docs.is_empty())
            .unwrap_or(true)
        {
            return Err(RuntimeError::InvalidManifest(
                "Invalid YAML format".to_string(),
            ));
        }

        debug!("Process engine: manifest validation passed");
        Ok(())
    }

    async fn deploy_workload(
        &self,
        manifest_id: &str,
        manifest_content: &[u8],
        config: &DeploymentConfig,
    ) -> RuntimeResult<WorkloadInfo> {
        info!(
            "Process engine: deploying workload for manifest_id: {}",
            manifest_id
        );

        // Check workload limit
        if self.workloads.read().await.len() >= MAX_PROCESS_WORKLOADS {
            return Err(RuntimeError::DeploymentFailed(format!(
                "Maximum workload limit ({}) reached",
                MAX_PROCESS_WORKLOADS
            )));
        }

        // Validate manifest first
        self.validate_manifest(manifest_content).await?;

        // Generate unique workload ID
        let workload_id = self.generate_workload_id(manifest_id);

        // Spawn the sidecar process
        let mut child = self
            .spawn_sidecar_process(&workload_id, manifest_id, manifest_content, config)
            .await?;

        let pid = child.id();

        // Give the process a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify process started successfully
        if !Self::is_process_running(&mut child).await {
            return Err(RuntimeError::DeploymentFailed(
                "Sidecar process exited immediately after spawn".to_string(),
            ));
        }

        // Parse manifest metadata
        let mut metadata = self.parse_manifest_metadata(manifest_content);
        if let Some(p) = pid {
            metadata.insert("pid".to_string(), p.to_string());
        }
        metadata.insert("runtime".to_string(), "process".to_string());

        // Create workload info
        let now = SystemTime::now();
        let workload_info = WorkloadInfo {
            id: workload_id.clone(),
            manifest_id: manifest_id.to_string(),
            status: WorkloadStatus::Running,
            metadata,
            created_at: now,
            updated_at: now,
            ports: vec![],
        };

        // Store the workload
        let process_workload = ProcessWorkload {
            info: workload_info.clone(),
            config: config.clone(),
            process: Some(child),
            pid,
        };

        self.workloads
            .write()
            .await
            .insert(workload_id.clone(), process_workload);

        info!(
            "Process engine: successfully deployed workload {} for manifest {} (PID: {:?})",
            workload_id, manifest_id, pid
        );

        Ok(workload_info)
    }

    async fn get_workload_status(&self, workload_id: &str) -> RuntimeResult<WorkloadInfo> {
        debug!(
            "Process engine: getting status for workload: {}",
            workload_id
        );

        let mut workloads = self.workloads.write().await;
        match workloads.get_mut(workload_id) {
            Some(workload) => {
                let mut info = workload.info.clone();
                info.updated_at = SystemTime::now();

                // Check if process is still running
                if let Some(ref mut process) = workload.process {
                    if Self::is_process_running(process).await {
                        info.status = WorkloadStatus::Running;
                    } else {
                        info.status = WorkloadStatus::Stopped;
                    }
                } else {
                    info.status = WorkloadStatus::Stopped;
                }

                workload.info = info.clone();
                Ok(info)
            }
            None => Err(RuntimeError::WorkloadNotFound(workload_id.to_string())),
        }
    }

    async fn list_workloads(&self) -> RuntimeResult<Vec<WorkloadInfo>> {
        debug!("Process engine: listing all workloads");

        let mut workloads = self.workloads.write().await;
        let mut workload_infos: Vec<WorkloadInfo> = Vec::new();

        for workload in workloads.values_mut() {
            let mut info = workload.info.clone();
            info.updated_at = SystemTime::now();

            // Check if process is still running
            if let Some(ref mut process) = workload.process {
                if Self::is_process_running(process).await {
                    info.status = WorkloadStatus::Running;
                } else {
                    info.status = WorkloadStatus::Stopped;
                }
            } else {
                info.status = WorkloadStatus::Stopped;
            }

            workload.info = info.clone();
            workload_infos.push(info);
        }

        // Sort by creation time for consistent ordering
        workload_infos.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        debug!("Process engine: found {} workloads", workload_infos.len());
        Ok(workload_infos)
    }

    async fn remove_workload(&self, workload_id: &str) -> RuntimeResult<()> {
        info!("Process engine: removing workload: {}", workload_id);

        let mut workloads = self.workloads.write().await;
        match workloads.remove(workload_id) {
            Some(mut workload) => {
                // Kill the process if still running
                if let Some(mut process) = workload.process.take() {
                    info!("Terminating process for workload {}", workload_id);
                    if let Err(e) = process.kill().await {
                        warn!("Failed to kill process for workload {}: {}", workload_id, e);
                    }
                }
                info!(
                    "Process engine: successfully removed workload {}",
                    workload_id
                );
                Ok(())
            }
            None => {
                warn!(
                    "Process engine: workload {} not found for removal",
                    workload_id
                );
                Err(RuntimeError::WorkloadNotFound(workload_id.to_string()))
            }
        }
    }

    async fn get_workload_logs(
        &self,
        workload_id: &str,
        _tail: Option<usize>,
    ) -> RuntimeResult<String> {
        debug!("Process engine: getting logs for workload: {}", workload_id);

        let workloads = self.workloads.read().await;
        match workloads.get(workload_id) {
            Some(workload) => {
                // For now, return a placeholder since we'd need to capture process output
                // In a real implementation, we'd capture stdout/stderr to a buffer
                let log_content = format!(
                    "Process workload {} (manifest: {}, PID: {:?})\nLogs not captured in current implementation.",
                    workload_id,
                    workload.info.manifest_id,
                    workload.pid
                );
                Ok(log_content)
            }
            None => Err(RuntimeError::WorkloadNotFound(workload_id.to_string())),
        }
    }

    async fn export_manifest(&self, workload_id: &str) -> RuntimeResult<Vec<u8>> {
        debug!(
            "Process engine: exporting manifest for workload: {}",
            workload_id
        );

        let workloads = self.workloads.read().await;
        match workloads.get(workload_id) {
            Some(workload) => {
                let metadata = &workload.info.metadata;
                let name = metadata.get("name").unwrap_or(&workload.info.id);
                let kind = metadata
                    .get("kind")
                    .map(|s| s.as_str())
                    .unwrap_or("Deployment");
                let api_version = metadata
                    .get("apiVersion")
                    .map(|s| s.as_str())
                    .unwrap_or("apps/v1");

                let manifest = format!(
                    r#"apiVersion: {}
kind: {}
metadata:
  name: {}
  labels:
    podmesh.io/workload-id: {}
    podmesh.io/manifest-id: {}
    podmesh.io/runtime: process
spec:
  replicas: {}
"#,
                    api_version,
                    kind,
                    name,
                    workload.info.id,
                    workload.info.manifest_id,
                    workload.config.replicas
                );

                Ok(manifest.into_bytes())
            }
            None => Err(RuntimeError::WorkloadNotFound(workload_id.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_engine_creation() {
        let engine = ProcessEngine::new();
        assert_eq!(engine.name(), "process");
        assert_eq!(engine.workload_count().await, 0);
    }

    #[tokio::test]
    async fn test_manifest_validation() {
        let engine = ProcessEngine::new();

        // Valid YAML manifest
        let valid_manifest = br#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
spec:
  containers:
  - name: nginx
    image: nginx:latest
"#;

        assert!(engine.validate_manifest(valid_manifest).await.is_ok());

        // Empty manifest
        let empty_manifest = b"";
        assert!(engine.validate_manifest(empty_manifest).await.is_err());
    }

    #[tokio::test]
    async fn test_parse_manifest_metadata() {
        let engine = ProcessEngine::new();

        let manifest = b"apiVersion: v1\nkind: Pod\nmetadata:\n  name: test-pod";
        let metadata = engine.parse_manifest_metadata(manifest);

        assert_eq!(metadata.get("apiVersion"), Some(&"v1".to_string()));
        assert_eq!(metadata.get("kind"), Some(&"Pod".to_string()));
        assert_eq!(metadata.get("name"), Some(&"test-pod".to_string()));
    }

    #[tokio::test]
    async fn test_generate_workload_id() {
        let engine = ProcessEngine::new();

        let id1 = engine.generate_workload_id("test-manifest");
        let id2 = engine.generate_workload_id("test-manifest");

        assert!(id1.starts_with("process-test-manifest-"));
        assert!(id2.starts_with("process-test-manifest-"));
        assert_ne!(id1, id2); // IDs should be unique
    }

    #[tokio::test]
    async fn test_port_allocation() {
        let config = ProcessEngineConfig {
            base_port: 20000,
            ..Default::default()
        };
        let engine = ProcessEngine::with_config(config);

        let port1 = engine.allocate_port();
        let port2 = engine.allocate_port();
        let port3 = engine.allocate_port();

        assert_eq!(port1, 20000);
        assert_eq!(port2, 20001);
        assert_eq!(port3, 20002);
    }
}
