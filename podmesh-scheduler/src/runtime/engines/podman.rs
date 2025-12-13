//! Podman runtime engine implementation
//!
//! This module provides a runtime engine that uses Podman to deploy and manage
//! Kubernetes manifests via `podman kube play`.

use crate::runtime::{
    DeploymentConfig, PortMapping, RuntimeEngine, RuntimeError, RuntimeResult, WorkloadInfo,
    WorkloadStatus,
};
use async_trait::async_trait;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use protocol::manifest_policy;
use protocol::manifest_yaml::{
    parse_yaml_documents_from_slice, parse_yaml_documents_from_str, serialize_yaml_documents,
};
use serde_yaml::Value;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::Stdio;
use std::sync::RwLock;
use tokio::process::Command;

const PODMESH_NETWORK_NAME: &str = "podmesh";

/// Podman runtime engine
pub struct PodmanEngine {
    podman_binary: String,
    podman_socket: Option<String>,
    force_remote: bool,
}

static PODMAN_SOCKET_OVERRIDE: Lazy<RwLock<Option<String>>> = Lazy::new(|| RwLock::new(None));
static PODMAN_FORCE_REMOTE: Lazy<RwLock<bool>> = Lazy::new(|| RwLock::new(false));

impl PodmanEngine {
    /// Create a new Podman engine instance
    pub fn new() -> Self {
        Self {
            podman_binary: "podman".to_string(),
            podman_socket: Self::detect_podman_socket(),
            force_remote: Self::is_force_remote(),
        }
    }

    /// Create a new Podman engine with custom binary path
    pub fn with_binary(binary_path: String) -> Self {
        Self {
            podman_binary: binary_path,
            podman_socket: Self::detect_podman_socket(),
            force_remote: Self::is_force_remote(),
        }
    }

    /// Configure the Podman runtime using CLI-provided parameters.
    pub fn configure_runtime(socket: Option<String>, force_remote: bool) {
        let normalized = socket.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(Self::normalize_socket(trimmed))
            }
        });

        let mut socket_guard = PODMAN_SOCKET_OVERRIDE
            .write()
            .expect("podman socket override rwlock poisoned");
        *socket_guard = normalized;

        let mut remote_guard = PODMAN_FORCE_REMOTE
            .write()
            .expect("podman force remote rwlock poisoned");
        *remote_guard = force_remote;
    }

    fn socket_override() -> Option<String> {
        PODMAN_SOCKET_OVERRIDE
            .read()
            .expect("podman socket override rwlock poisoned")
            .clone()
    }

    fn is_force_remote() -> bool {
        *PODMAN_FORCE_REMOTE
            .read()
            .expect("podman force remote rwlock poisoned")
    }

    fn normalize_socket(value: &str) -> String {
        if value.contains("://") {
            value.to_string()
        } else {
            format!("unix://{}", value)
        }
    }

    /// Validate that a socket path exists and is accessible
    fn validate_socket(socket_url: &str) -> bool {
        // Extract the filesystem path from the socket URL
        let path_str = if let Some(stripped) = socket_url.strip_prefix("unix://") {
            stripped
        } else if socket_url.contains("://") {
            // Non-unix socket URLs (tcp://, http://, etc.) can't be validated locally
            debug!(
                "Socket URL {} is not a unix socket, skipping filesystem validation",
                socket_url
            );
            return true;
        } else {
            socket_url
        };

        let path = Path::new(path_str);

        // Check if the socket file exists
        match fs::metadata(path) {
            Ok(metadata) => {
                // Verify it's a socket file type
                let file_type = metadata.file_type();
                if !file_type.is_socket() {
                    warn!("Path {} exists but is not a socket file", path_str);
                    return false;
                }

                // Check if we have read/write permissions
                // For Unix sockets, we need to be able to connect to them
                let mode = metadata.mode();
                let is_readable = mode & 0o400 != 0; // Owner read
                let is_writable = mode & 0o200 != 0; // Owner write

                if !is_readable || !is_writable {
                    warn!(
                        "Socket {} exists but lacks read/write permissions (mode: {:o})",
                        path_str, mode
                    );
                    return false;
                }

                debug!("Socket {} exists and is accessible", path_str);
                true
            }
            Err(e) => {
                debug!("Socket {} is not accessible: {}", path_str, e);
                false
            }
        }
    }

    /// Detect a podman socket URL from configuration, environment, or common locations
    fn detect_podman_socket() -> Option<String> {
        Self::socket_override()
    }

    /// Execute a podman command and return the output
    async fn execute_command(&self, args: &[&str]) -> RuntimeResult<String> {
        if let Some(socket) = &self.podman_socket {
            // Check if socket exists and is accessible before attempting connection
            if !Self::validate_socket(socket) {
                if self.force_remote {
                    return Err(RuntimeError::CommandFailed(format!(
                        "Podman socket {} is not available and fallback is disabled",
                        socket
                    )));
                }

                info!(
                    "Podman socket {} is not available, falling back to local CLI",
                    socket
                );
                return self.execute_command_local(args).await;
            }

            match self.execute_command_with_socket(args, socket).await {
                Ok(output) => return Ok(output),
                Err(err) => {
                    if self.force_remote {
                        return Err(err);
                    }

                    if let RuntimeError::CommandFailed(stderr) = &err {
                        if Self::is_socket_connection_error(stderr) {
                            warn!(
                                "Podman socket command failed ({}). Falling back to local CLI...",
                                stderr.trim()
                            );
                        } else {
                            return Err(err);
                        }
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        if self.force_remote {
            return Err(RuntimeError::CommandFailed(
                "Podman socket command failed and fallback disabled".to_string(),
            ));
        }

        self.execute_command_local(args).await
    }

    /// Execute podman command against configured socket
    async fn execute_command_with_socket(
        &self,
        args: &[&str],
        socket: &str,
    ) -> RuntimeResult<String> {
        debug!(
            "Executing podman (remote) command via {}: {} {}",
            socket,
            self.podman_binary,
            args.join(" ")
        );

        let output = Command::new(&self.podman_binary)
            .arg("--remote")
            .arg("--url")
            .arg(socket)
            .args(args)
            .env("CONTAINER_HOST", socket)
            .env("PODMAN_HOST", socket)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        Self::process_command_output(output)
    }

    /// Execute podman command locally (no socket)
    async fn execute_command_local(&self, args: &[&str]) -> RuntimeResult<String> {
        debug!(
            "Executing podman (local) command: {} {}",
            self.podman_binary,
            args.join(" ")
        );

        let output = Command::new(&self.podman_binary)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        Self::process_command_output(output)
    }

    /// Helper to process podman command output
    fn process_command_output(output: std::process::Output) -> RuntimeResult<String> {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            debug!("Podman command succeeded: {}", stdout.trim());
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            error!("Podman command failed: {}", stderr);
            Err(RuntimeError::CommandFailed(stderr))
        }
    }

    /// Detect if a failed command looks like a socket connectivity issue
    fn is_socket_connection_error(stderr: &str) -> bool {
        let lower = stderr.to_ascii_lowercase();
        lower.contains("dial unix")
            || lower.contains("connect:")
            || lower.contains("connection refused")
            || lower.contains("no such file or directory")
    }

    /// Parse Kubernetes manifest to extract metadata
    fn parse_manifest_metadata(
        &self,
        manifest_content: &[u8],
    ) -> RuntimeResult<HashMap<String, String>> {
        let mut metadata = HashMap::new();

        if let Ok(docs) = parse_yaml_documents_from_slice(manifest_content) {
            for doc in docs {
                if let Some(kind) = doc.get("kind").and_then(|k| k.as_str()) {
                    metadata.insert("kind".to_string(), kind.to_string());
                }
                if let Some(api_version) = doc.get("apiVersion").and_then(|v| v.as_str()) {
                    metadata.insert("apiVersion".to_string(), api_version.to_string());
                }
                if let Some(meta) = doc.get("metadata") {
                    if let Some(name) = meta.get("name").and_then(|n| n.as_str()) {
                        metadata.insert("name".to_string(), name.to_string());
                    }
                    if let Some(namespace) = meta.get("namespace").and_then(|n| n.as_str()) {
                        metadata.insert("namespace".to_string(), namespace.to_string());
                    }
                }

                if !metadata.is_empty() {
                    break;
                }
            }
        }

        Ok(metadata)
    }

    /// Extract port mappings from podman pod inspect output
    async fn extract_port_mappings(&self, pod_name: &str) -> RuntimeResult<Vec<PortMapping>> {
        // Try the pod name with -pod suffix first (most common)
        let pod_name_with_suffix = format!("{}-pod", pod_name);

        match self
            .execute_command(&["pod", "inspect", &pod_name_with_suffix, "--format", "json"])
            .await
        {
            Ok(_output) => {
                // Parse JSON output to extract port mappings
                // This is a simplified implementation - in practice you'd parse the full JSON
                let ports = Vec::new();
                debug!(
                    "Extracted {} port mappings for pod {}",
                    ports.len(),
                    pod_name_with_suffix
                );
                return Ok(ports);
            }
            Err(_) => {
                debug!(
                    "Failed to inspect pod with suffix: {}",
                    pod_name_with_suffix
                );
            }
        }

        // Try without suffix as fallback
        match self
            .execute_command(&["pod", "inspect", pod_name, "--format", "json"])
            .await
        {
            Ok(_output) => {
                let ports = Vec::new();
                debug!(
                    "Extracted {} port mappings for pod {}",
                    ports.len(),
                    pod_name
                );
                Ok(ports)
            }
            Err(e) => {
                debug!("Failed to inspect pod {}: {}", pod_name, e);
                // Return empty ports if inspection fails - this is not a critical error
                Ok(Vec::new())
            }
        }
    }

    /// Generate a unique workload ID based on manifest ID only
    fn generate_workload_id(&self, manifest_id: &str, _manifest_content: &[u8]) -> String {
        // Use consistent naming with pod name - just manifest_id based
        format!("podmesh-{}", manifest_id)
    }

    /// Create a temporary file with the manifest content, modifying pod name to use manifest_id
    async fn create_temp_manifest_file(
        &self,
        manifest_content: &[u8],
        manifest_id: &str,
    ) -> RuntimeResult<std::path::PathBuf> {
        use tokio::io::AsyncWriteExt;

        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!("podmesh-manifest-{}.yaml", uuid::Uuid::new_v4()));

        let mut docs = parse_yaml_documents_from_slice(manifest_content)
            .map_err(|e| RuntimeError::InvalidManifest(format!("YAML parse error: {}", e)))?;

        if docs.is_empty() {
            return Err(RuntimeError::InvalidManifest(
                "Manifest did not contain any YAML documents".to_string(),
            ));
        }

        let pod_name = format!("podmesh-{}", manifest_id);
        let mut renamed = false;
        for doc in docs.iter_mut() {
            if rename_manifest_doc(doc, &pod_name) {
                renamed = true;
                break;
            }
        }

        if !renamed {
            warn!(
                "No workload resource found while preparing manifest {}; pod name unchanged",
                manifest_id
            );
        }

        let modified_manifest = serialize_yaml_documents(&docs)
            .map_err(|e| RuntimeError::InvalidManifest(format!("YAML serialize error: {}", e)))?;

        let mut file = tokio::fs::File::create(&temp_file).await?;
        file.write_all(modified_manifest.as_bytes()).await?;
        file.flush().await?;

        debug!(
            "Created temporary manifest file: {:?} with pod name: {}",
            temp_file, pod_name
        );
        Ok(temp_file)
    }

    /// Clean up temporary manifest file
    async fn cleanup_temp_file(&self, path: &std::path::Path) {
        if let Err(e) = tokio::fs::remove_file(path).await {
            warn!("Failed to clean up temporary file {:?}: {}", path, e);
        } else {
            debug!("Cleaned up temporary file: {:?}", path);
        }
    }

    /// Ensure the dedicated podmesh podman network exists, creating it if needed
    async fn ensure_podmesh_network(&self) -> RuntimeResult<()> {
        if self
            .execute_command(&["network", "exists", PODMESH_NETWORK_NAME])
            .await
            .is_ok()
        {
            return Ok(());
        }

        info!(
            "Podman network '{}' missing; attempting to create it",
            PODMESH_NETWORK_NAME
        );

        match self
            .execute_command(&["network", "create", PODMESH_NETWORK_NAME])
            .await
        {
            Ok(output) => {
                info!(
                    "Created podman network '{}': {}",
                    PODMESH_NETWORK_NAME,
                    output.trim()
                );
                Ok(())
            }
            Err(err) => {
                error!(
                    "Failed to create podman network '{}': {}",
                    PODMESH_NETWORK_NAME, err
                );
                Err(err)
            }
        }
    }
}

fn rename_manifest_doc(doc: &mut Value, pod_name: &str) -> bool {
    // Only rename pod-spec workloads so supporting resources (ConfigMaps, PVCs, etc.)
    // retain their original names referenced by the workload.
    let kind = doc
        .get("kind")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .unwrap_or_default();

    if !matches!(
        kind.as_str(),
        "Pod" | "Deployment" | "ReplicaSet" | "DaemonSet" | "StatefulSet"
    ) {
        return false;
    }

    let mut updated = false;

    if let Some(metadata) = doc
        .get_mut("metadata")
        .and_then(|value| value.as_mapping_mut())
    {
        metadata.insert(
            Value::String("name".to_string()),
            Value::String(pod_name.to_string()),
        );
        updated = true;
    }

    if kind == "Pod" {
        return updated;
    }

    if let Some(spec) = doc.get_mut("spec").and_then(|value| value.as_mapping_mut()) {
        if let Some(template) = spec
            .get_mut(&Value::String("template".to_string()))
            .and_then(|value| value.as_mapping_mut())
        {
            if let Some(template_metadata) = template
                .get_mut(&Value::String("metadata".to_string()))
                .and_then(|value| value.as_mapping_mut())
            {
                template_metadata.insert(
                    Value::String("name".to_string()),
                    Value::String(format!("{}-pod", pod_name)),
                );
                updated = true;
            }
        }
    }

    updated
}

impl Default for PodmanEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RuntimeEngine for PodmanEngine {
    fn name(&self) -> &str {
        "podman"
    }

    async fn is_available(&self) -> bool {
        let check_args = ["--version"];

        if let Some(socket) = &self.podman_socket {
            // Check if socket is accessible before trying to use it
            if !Self::validate_socket(socket) {
                if self.force_remote {
                    debug!(
                        "Podman socket {} not available and fallback disabled; marking unavailable",
                        socket
                    );
                    return false;
                }
                debug!(
                    "Podman socket {} not available, checking local availability",
                    socket
                );
                return self.execute_command_local(&check_args).await.is_ok();
            }

            return self
                .execute_command_with_socket(&check_args, socket)
                .await
                .is_ok();
        }

        if self.force_remote {
            debug!("Podman socket forced but not configured; marking unavailable");
            return false;
        }

        self.execute_command_local(&check_args).await.is_ok()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn validate_manifest(&self, manifest_content: &[u8]) -> RuntimeResult<()> {
        let manifest_str = String::from_utf8_lossy(manifest_content);
        
        // First validate basic YAML structure
        let docs = parse_yaml_documents_from_str(&manifest_str)
            .map_err(|e| RuntimeError::InvalidManifest(format!("YAML parse error: {}", e)))?;

        if docs.is_empty() {
            return Err(RuntimeError::InvalidManifest(
                "Manifest did not contain any YAML documents".to_string(),
            ));
        }

        for (idx, doc) in docs.iter().enumerate() {
            if doc.get("apiVersion").is_none() {
                return Err(RuntimeError::InvalidManifest(format!(
                    "Document {} missing apiVersion field",
                    idx + 1
                )));
            }
            if doc.get("kind").is_none() {
                return Err(RuntimeError::InvalidManifest(format!(
                    "Document {} missing kind field",
                    idx + 1
                )));
            }
        }

        // Then validate against security policies
        let policy_result = manifest_policy::validate_manifest(&manifest_str)
            .map_err(|e| RuntimeError::InvalidManifest(format!("Policy validation error: {}", e)))?;
        
        if !policy_result.allowed {
            let violations = if policy_result.violations.is_empty() {
                "policy violation".to_string()
            } else {
                policy_result.violations.join("; ")
            };
            return Err(RuntimeError::InvalidManifest(format!(
                "Policy validation failed: {}",
                violations
            )));
        }

        info!("Manifest validation passed for {} documents", docs.len());
        Ok(())
    }

    async fn deploy_workload(
        &self,
        manifest_id: &str,
        manifest_content: &[u8],
        config: &DeploymentConfig,
    ) -> RuntimeResult<WorkloadInfo> {
        info!("Deploying workload for manifest_id: {}", manifest_id);

        // Validate manifest first
        self.validate_manifest(manifest_content).await?;

        // Generate unique workload ID
        let workload_id = self.generate_workload_id(manifest_id, manifest_content);

        // Create temporary manifest file with modified pod name
        let temp_file = self
            .create_temp_manifest_file(manifest_content, manifest_id)
            .await?;

        // Ensure the dedicated podmesh network exists before deployment
        self.ensure_podmesh_network().await?;

        // Build podman kube play command
        let mut args = vec!["kube", "play"];

        // Add --replace flag to overwrite existing pods
        args.push("--replace");

        // Always ensure workloads run inside the podmesh network
        args.extend(["--network", PODMESH_NETWORK_NAME]);

        // Add replicas if specified and > 1
        let replicas_str;
        if config.replicas > 1 {
            replicas_str = config.replicas.to_string();
            args.extend(&["--replicas", &replicas_str]);
        }

        // Add resource limits if specified
        let memory_str;
        if let Some(memory) = config.resources.memory {
            memory_str = format!("{}b", memory);
            args.extend(&["--memory", &memory_str]);
        }

        let cpu_str;
        if let Some(cpu) = config.resources.cpu {
            cpu_str = format!("{}", cpu);
            args.extend(&["--cpus", &cpu_str]);
        }

        // Add environment variables
        let mut env_strings = Vec::new();
        for (key, value) in &config.env {
            env_strings.push(format!("{}={}", key, value));
        }
        /*for env_str in &env_strings {
            args.extend(&["--env", env_str]);
        }*/

        // Add runtime-specific options
        for (key, value) in &config.runtime_options {
            match key.as_str() {
                "network" => {
                    if value != PODMESH_NETWORK_NAME {
                        warn!(
                            "Ignoring runtime network override '{}' for manifest {}; using {}",
                            value, manifest_id, PODMESH_NETWORK_NAME
                        );
                    }
                }
                "volume" => args.extend(&["--volume", value]),
                "security-opt" => args.extend(&["--security-opt", value]),
                _ => debug!("Ignoring unknown runtime option: {}={}", key, value),
            }
        }

        // Add the manifest file
        args.push(temp_file.to_str().ok_or_else(|| {
            RuntimeError::InvalidManifest("Invalid temporary file path".to_string())
        })?);

        // Execute the deployment
        match self.execute_command(&args).await {
            Ok(output) => {
                info!(
                    "Podman kube play succeeded for workload {}: {}",
                    workload_id,
                    output.trim()
                );

                // Clean up temporary file
                self.cleanup_temp_file(&temp_file).await;

                // Parse manifest metadata
                let metadata = self
                    .parse_manifest_metadata(manifest_content)
                    .unwrap_or_default();

                // Use the manifest_id-based pod name (consistent with our modified manifest)
                let pod_name = format!("podmesh-{}", manifest_id);

                // Get port mappings
                let ports = self
                    .extract_port_mappings(&pod_name)
                    .await
                    .unwrap_or_default();

                let now = std::time::SystemTime::now();
                Ok(WorkloadInfo {
                    id: workload_id,
                    manifest_id: manifest_id.to_string(),
                    status: WorkloadStatus::Running,
                    metadata,
                    created_at: now,
                    updated_at: now,
                    ports,
                })
            }
            Err(e) => {
                error!(
                    "Podman kube play failed for workload {}: {}",
                    workload_id, e
                );

                // Clean up temporary file
                self.cleanup_temp_file(&temp_file).await;

                Err(RuntimeError::DeploymentFailed(format!(
                    "Podman deployment failed: {}",
                    e
                )))
            }
        }
    }

    /// Deploy a workload with local peer ID tracking
    async fn deploy_workload_with_peer(
        &self,
        manifest_id: &str,
        manifest_content: &[u8],
        config: &DeploymentConfig,
        local_peer_id: libp2p::PeerId,
    ) -> RuntimeResult<WorkloadInfo> {
        // Validate the manifest first
        self.validate_manifest(manifest_content).await?;

        // Generate unique workload ID
        let workload_id = self.generate_workload_id(manifest_id, manifest_content);

        // Create temporary manifest file with modified pod name
        let temp_file = self
            .create_temp_manifest_file(manifest_content, manifest_id)
            .await?;

        // Ensure the dedicated podmesh network exists before deployment
        self.ensure_podmesh_network().await?;

        // Prepare podman command
        let mut args = vec!["kube", "play"];

        // Add --replace flag to overwrite existing pods
        args.push("--replace");

        // Always ensure workloads run inside the podmesh network
        args.extend(["--network", PODMESH_NETWORK_NAME]);

        // Add environment variables
        let mut env_strings = Vec::new();
        for (key, value) in &config.env {
            env_strings.push(format!("{}={}", key, value));
        }

        /*if !env_strings.is_empty() {
            for env in &env_strings {
                args.extend(&["--env", env]);
            }
        }*/

        // Add the manifest file
        args.push(temp_file.to_str().unwrap());

        // Add runtime-specific options (excluding network override)
        for (key, value) in &config.runtime_options {
            match key.as_str() {
                "network" => {
                    if value != PODMESH_NETWORK_NAME {
                        warn!(
                            "Ignoring runtime network override '{}' for manifest {}; using {}",
                            value, manifest_id, PODMESH_NETWORK_NAME
                        );
                    }
                }
                "volume" => args.extend(&["--volume", value]),
                "security-opt" => args.extend(&["--security-opt", value]),
                _ => debug!("Ignoring unknown runtime option: {}={}", key, value),
            }
        }

        // Execute the deployment
        match self.execute_command(&args).await {
            Ok(output) => {
                info!(
                    "Podman kube play succeeded for workload {}: {}",
                    workload_id,
                    output.trim()
                );

                // Clean up temporary file
                self.cleanup_temp_file(&temp_file).await;

                // Parse manifest metadata
                let mut metadata = self
                    .parse_manifest_metadata(manifest_content)
                    .unwrap_or_default();

                // Add local peer ID to metadata
                metadata.insert("local_peer_id".to_string(), local_peer_id.to_string());

                // Use the manifest_id-based pod name (consistent with our modified manifest)
                let pod_name = format!("podmesh-{}", manifest_id);

                // Get port mappings
                let ports = self
                    .extract_port_mappings(&pod_name)
                    .await
                    .unwrap_or_default();

                let now = std::time::SystemTime::now();
                Ok(WorkloadInfo {
                    id: workload_id,
                    manifest_id: manifest_id.to_string(),
                    status: WorkloadStatus::Running,
                    metadata,
                    created_at: now,
                    updated_at: now,
                    ports,
                })
            }
            Err(e) => {
                error!(
                    "Podman kube play failed for workload {}: {}",
                    workload_id, e
                );

                // Clean up temporary file
                self.cleanup_temp_file(&temp_file).await;

                Err(RuntimeError::DeploymentFailed(format!(
                    "Podman deployment failed: {}",
                    e
                )))
            }
        }
    }

    async fn get_workload_status(&self, workload_id: &str) -> RuntimeResult<WorkloadInfo> {
        debug!("Getting status for workload: {}", workload_id);

        // Try to find the pod by listing all pods and matching by labels or names
        let _output = self
            .execute_command(&["pod", "ls", "--format", "json"])
            .await?;

        // Parse JSON output to find our workload
        // This is a simplified implementation - in practice you'd parse the full JSON
        // and match based on labels or naming conventions

        // For now, return a basic status
        let now = std::time::SystemTime::now();
        Ok(WorkloadInfo {
            id: workload_id.to_string(),
            manifest_id: "unknown".to_string(),
            status: WorkloadStatus::Unknown,
            metadata: HashMap::new(),
            created_at: now,
            updated_at: now,
            ports: Vec::new(),
        })
    }

    async fn list_workloads(&self) -> RuntimeResult<Vec<WorkloadInfo>> {
        debug!("Listing all workloads");

        let output = self
            .execute_command(&["pod", "ls", "--format", "json"])
            .await?;

        let mut workloads = Vec::new();

        // Parse JSON output to create WorkloadInfo objects
        if !output.trim().is_empty() {
            match serde_json::from_str::<serde_json::Value>(&output) {
                Ok(json) => {
                    if let Some(pods_array) = json.as_array() {
                        for pod in pods_array {
                            if let Some(pod_name) = pod.get("Name").and_then(|n| n.as_str()) {
                                // Only include pods that match our naming convention "podmesh-*"
                                if pod_name.starts_with("podmesh-") {
                                    // Extract manifest_id from pod name
                                    let manifest_id = if pod_name.ends_with("-pod") {
                                        // Remove both "podmesh-" prefix and "-pod" suffix
                                        pod_name
                                            .strip_prefix("podmesh-")
                                            .unwrap_or(pod_name)
                                            .strip_suffix("-pod")
                                            .unwrap_or(pod_name)
                                            .to_string()
                                    } else {
                                        // Remove "podmesh-" prefix only
                                        pod_name
                                            .strip_prefix("podmesh-")
                                            .unwrap_or(pod_name)
                                            .to_string()
                                    };

                                    // Parse pod status
                                    let status = match pod.get("Status").and_then(|s| s.as_str()) {
                                        Some("Running") => WorkloadStatus::Running,
                                        Some("Stopped") | Some("Exited") => WorkloadStatus::Stopped,
                                        Some("Error") => {
                                            WorkloadStatus::Failed("Pod in error state".to_string())
                                        }
                                        Some("Failed") => {
                                            WorkloadStatus::Failed("Pod failed".to_string())
                                        }
                                        _ => WorkloadStatus::Unknown,
                                    };

                                    // Extract metadata from pod labels if available
                                    let mut metadata = HashMap::new();
                                    if let Some(labels) = pod.get("Labels") {
                                        if let Some(labels_obj) = labels.as_object() {
                                            for (key, value) in labels_obj {
                                                if let Some(value_str) = value.as_str() {
                                                    metadata
                                                        .insert(key.clone(), value_str.to_string());
                                                }
                                            }
                                        }
                                    }

                                    // Parse created timestamp
                                    let created_at = pod
                                        .get("Created")
                                        .and_then(|c| c.as_str())
                                        .and_then(|created_str| {
                                            // Try to parse RFC3339 timestamp
                                            std::time::SystemTime::UNIX_EPOCH.checked_add(
                                                std::time::Duration::from_secs(
                                                    created_str.parse::<u64>().unwrap_or(0),
                                                ),
                                            )
                                        })
                                        .unwrap_or_else(std::time::SystemTime::now);

                                    let workload_info = WorkloadInfo {
                                        id: format!("podmesh-{}", manifest_id),
                                        manifest_id,
                                        status,
                                        metadata,
                                        created_at,
                                        updated_at: std::time::SystemTime::now(),
                                        ports: Vec::new(), // Port mappings would need separate inspection
                                    };

                                    debug!(
                                        "Found podmesh workload: {} (pod: {})",
                                        workload_info.id, pod_name
                                    );
                                    workloads.push(workload_info);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("Failed to parse JSON output: {}", e);
                    debug!("Raw output: {}", output);
                }
            }
        }

        debug!("Found {} workloads", workloads.len());
        Ok(workloads)
    }

    async fn remove_workload(&self, workload_id: &str) -> RuntimeResult<()> {
        info!("Removing workload: {}", workload_id);

        // For our naming convention, the workload_id is "podmesh-{manifest_id}"
        // But Podman creates pods with "-pod" suffix, so we need to try both forms
        let pod_name_with_suffix = format!("{}-pod", workload_id);

        // Try to remove by pod name with suffix first (most likely to succeed)
        match self
            .execute_command(&["pod", "rm", "-f", &pod_name_with_suffix])
            .await
        {
            Ok(output) => {
                info!(
                    "Successfully removed pod {}: {}",
                    pod_name_with_suffix,
                    output.trim()
                );
                return Ok(());
            }
            Err(e) => {
                debug!(
                    "Failed to remove pod with suffix {}: {}",
                    pod_name_with_suffix, e
                );
            }
        }

        // Try to remove by exact workload_id
        match self
            .execute_command(&["pod", "rm", "-f", workload_id])
            .await
        {
            Ok(output) => {
                info!(
                    "Successfully removed workload {}: {}",
                    workload_id,
                    output.trim()
                );
                return Ok(());
            }
            Err(e) => {
                debug!("Direct removal failed: {}", e);
            }
        }

        // If both specific removals fail, try to find and remove by pattern
        warn!(
            "Specific removals failed, trying pattern match for: {}",
            workload_id
        );

        // List pods and find ones that match our naming pattern
        let output = self
            .execute_command(&[
                "pod",
                "ls",
                "-q",
                "--filter",
                &format!("name={}", workload_id),
            ])
            .await?;

        for line in output.lines() {
            let pod_id = line.trim();
            if !pod_id.is_empty() {
                if let Err(e) = self.execute_command(&["pod", "rm", "-f", pod_id]).await {
                    warn!("Failed to remove pod {}: {}", pod_id, e);
                } else {
                    info!("Successfully removed pod: {}", pod_id);
                }
            }
        }

        Ok(())
    }

    async fn get_workload_logs(
        &self,
        workload_id: &str,
        tail: Option<usize>,
    ) -> RuntimeResult<String> {
        debug!("Getting logs for workload: {}", workload_id);

        let mut args = vec!["pod", "logs"];

        let tail_str;
        if let Some(tail_lines) = tail {
            tail_str = tail_lines.to_string();
            args.extend(&["--tail", &tail_str]);
        }

        args.push(workload_id);

        match self.execute_command(&args).await {
            Ok(logs) => {
                debug!(
                    "Retrieved {} bytes of logs for workload {}",
                    logs.len(),
                    workload_id
                );
                Ok(logs)
            }
            Err(e) => {
                warn!("Failed to get logs for workload {}: {}", workload_id, e);
                Ok(format!("Failed to retrieve logs: {}", e))
            }
        }
    }

    async fn export_manifest(&self, workload_id: &str) -> RuntimeResult<Vec<u8>> {
        info!("Exporting manifest for workload: {}", workload_id);

        // Podman creates pods with different naming patterns:
        // 1. For our workload_id "podmesh-manifest-123", it might create:
        //    - "podmesh-manifest-123-pod" (most common)
        //    - "podmesh-manifest-123" (exact match)
        //    - Other variations based on the original manifest

        let pod_name_variations = vec![
            format!("{}-pod", workload_id), // Most common pattern
            workload_id.to_string(),        // Exact match
        ];

        let mut last_error = None;

        for pod_name in &pod_name_variations {
            debug!("Trying to export manifest for pod: {}", pod_name);

            match self.execute_command(&["generate", "kube", pod_name]).await {
                Ok(manifest_yaml) => {
                    info!(
                        "Successfully exported manifest for workload {} (pod: {})",
                        workload_id, pod_name
                    );
                    debug!(
                        "Exported manifest ({} bytes): {}",
                        manifest_yaml.len(),
                        manifest_yaml.trim()
                    );
                    return Ok(manifest_yaml.into_bytes());
                }
                Err(e) => {
                    debug!("Failed to export manifest for pod {}: {}", pod_name, e);
                    last_error = Some(e);
                }
            }
        }

        // If all variations failed, try to find the actual pod name by listing
        debug!("All direct attempts failed, trying to find pod by listing");

        match self
            .execute_command(&[
                "pod",
                "ls",
                "--format",
                "{{.Name}}",
                "--filter",
                &format!("name={}", workload_id),
            ])
            .await
        {
            Ok(output) => {
                for line in output.lines() {
                    let actual_pod_name = line.trim();
                    if !actual_pod_name.is_empty() && actual_pod_name.contains(workload_id) {
                        debug!("Found actual pod name: {}", actual_pod_name);

                        match self
                            .execute_command(&["generate", "kube", actual_pod_name])
                            .await
                        {
                            Ok(manifest_yaml) => {
                                info!(
                                    "Successfully exported manifest for workload {} (actual pod: {})",
                                    workload_id, actual_pod_name
                                );
                                return Ok(manifest_yaml.into_bytes());
                            }
                            Err(e) => {
                                debug!(
                                    "Failed to export manifest for actual pod {}: {}",
                                    actual_pod_name, e
                                );
                                last_error = Some(e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Failed to list pods: {}", e);
                last_error = Some(e);
            }
        }

        // All attempts failed
        let error_msg = match last_error {
            Some(e) => format!(
                "Failed to export manifest for workload {}: {}",
                workload_id, e
            ),
            None => format!("No running pod found for workload {}", workload_id),
        };

        error!("{}", error_msg);
        Err(RuntimeError::WorkloadNotFound(error_msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;
    use serial_test::serial;

    #[tokio::test]
    #[serial]
    async fn test_podman_engine_creation() {
        PodmanEngine::configure_runtime(None, false);
        let engine = PodmanEngine::new();
        assert_eq!(engine.name(), "podman");
    }

    #[tokio::test]
    async fn test_manifest_validation() {
        let engine = PodmanEngine::new();

        // Valid manifest
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

        // Invalid manifest (missing apiVersion)
        let invalid_manifest = br#"
kind: Pod
metadata:
  name: test-pod
spec:
  containers:
  - name: nginx
    image: nginx:latest
"#;

        assert!(engine.validate_manifest(invalid_manifest).await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_parse_manifest_metadata() {
        PodmanEngine::configure_runtime(None, false);
        let engine = PodmanEngine::new();

        let manifest = br#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
  namespace: default
spec:
  containers:
  - name: nginx
    image: nginx:latest
"#;

        let metadata = engine.parse_manifest_metadata(manifest).unwrap();
        assert_eq!(metadata.get("kind"), Some(&"Pod".to_string()));
        assert_eq!(metadata.get("apiVersion"), Some(&"v1".to_string()));
        assert_eq!(metadata.get("name"), Some(&"test-pod".to_string()));
        assert_eq!(metadata.get("namespace"), Some(&"default".to_string()));
    }

    #[tokio::test]
    #[serial]
    async fn test_workload_id_generation() {
        PodmanEngine::configure_runtime(None, false);
        let engine = PodmanEngine::new();

        let manifest1 = b"apiVersion: v1\nkind: Pod\nmetadata:\n  name: test-pod";
        let manifest2 = b"apiVersion: v1\nkind: Pod\nmetadata:\n  name: different-pod";
        let manifest_without_name = b"apiVersion: v1\nkind: Pod";

        let id1 = engine.generate_workload_id("manifest-123", manifest1);
        let id2 = engine.generate_workload_id("manifest-123", manifest1);
        let id3 = engine.generate_workload_id("manifest-456", manifest2);
        let id4 = engine.generate_workload_id("manifest-789", manifest_without_name);

        // IDs should be identical for the same manifest and ID
        assert_eq!(id1, id2);
        assert_eq!(id1, "podmesh-manifest-123");

        // Different manifest IDs should produce different workload IDs
        assert_ne!(id1, id3);
        assert_eq!(id3, "podmesh-manifest-456");

        // Manifest without name should still use manifest_id only
        assert_eq!(id4, "podmesh-manifest-789");
    }

    #[tokio::test]
    #[serial]
    async fn test_force_remote_configuration() {
        PodmanEngine::configure_runtime(Some("/run/podman/podman.sock".to_string()), true);
        let engine = PodmanEngine::new();
        assert!(engine.force_remote);
        assert_eq!(
            engine.podman_socket.as_deref(),
            Some("unix:///run/podman/podman.sock")
        );
    }

    #[test]
    fn test_rename_manifest_doc_skips_configmap() {
        let mut config_map: Value = serde_yaml::from_str(
            r#"
apiVersion: v1
kind: ConfigMap
metadata:
    name: nginx-custom-content
data:
    index.html: hello
"#,
        )
        .unwrap();

        let renamed = super::rename_manifest_doc(&mut config_map, "podmesh-demo");

        assert!(!renamed, "ConfigMap should not be renamed");
        let name = config_map
            .get("metadata")
            .and_then(|meta| meta.get("name"))
            .and_then(|name| name.as_str())
            .unwrap();
        assert_eq!(name, "nginx-custom-content");
    }

    #[test]
    fn test_rename_manifest_doc_updates_deployment() {
        let mut deployment: Value = serde_yaml::from_str(
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
    name: my-nginx
spec:
    template:
        metadata:
            name: my-nginx-pod
"#,
        )
        .unwrap();

        let renamed = super::rename_manifest_doc(&mut deployment, "podmesh-demo");

        assert!(renamed);

        let top_name = deployment
            .get("metadata")
            .and_then(|meta| meta.get("name"))
            .and_then(|name| name.as_str())
            .unwrap();
        assert_eq!(top_name, "podmesh-demo");

        let template_name = deployment
            .get("spec")
            .and_then(|spec| spec.get("template"))
            .and_then(|template| template.get("metadata"))
            .and_then(|meta| meta.get("name"))
            .and_then(|name| name.as_str())
            .unwrap();
        assert_eq!(template_name, "podmesh-demo-pod");
    }
}
