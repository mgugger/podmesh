//! Private control endpoint inside the workload boundary.
//!
//! Podman transports inspection via exec; a future guest runtime can transport
//! the same bounded protocol. This endpoint neither accepts machine-plane
//! membership nor issues credentials. Self-signed reports must not be mistaken
//! for owner-authorized discovery records.

use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use iroh::Endpoint;
use iroh_support::NodeIdentity;
use protocol::workload_connector::{
    CONNECTOR_CONTROL_TIMEOUT, ConnectorError, ConnectorIdentityReport, ConnectorRequest,
    ConnectorResponse, read_frame, write_frame,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const MAX_CONTROL_CONNECTIONS: usize = 8;
const RECORD_LIFETIME_SECS: u64 = 300;

#[derive(Clone)]
pub(crate) struct ConnectorContext {
    pub owner: String,
    pub workload_name: String,
    pub replica_index: u32,
    pub replica_count: u32,
    pub endpoint: Endpoint,
    pub identity: Arc<NodeIdentity>,
    pub authorization: Arc<crate::authorization::WorkloadAuthorization>,
    pub network: Option<Arc<crate::workload_network::WorkloadNetwork>>,
}

impl ConnectorContext {
    fn inspect(&self, request: &ConnectorRequest) -> Result<ConnectorIdentityReport> {
        request.validate()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let owner = crypto::b64_decode(&self.owner)?;
        let addr = self.endpoint.addr();
        let endpoint = protocol::EndpointRecord {
            version: protocol::ENDPOINT_RECORD_VERSION,
            endpoint_id: self.endpoint.id().as_bytes().to_vec(),
            relay_url: addr.relay_urls().next().map(ToString::to_string),
            direct_addresses: addr
                .ip_addrs()
                .take(protocol::MAX_ENDPOINT_DIRECT_ADDRESSES)
                .map(ToString::to_string)
                .collect(),
            signing_pubkey: String::new(),
            issued_at_secs: now,
            expires_at_secs: now.saturating_add(RECORD_LIFETIME_SECS),
            signature: String::new(),
        }
        .sign(
            self.identity.signing_public(),
            self.identity.signing_private(),
            now,
        )?;
        let credential = self.authorization.current();
        ConnectorIdentityReport {
            owner_public_key_b64: self.owner.clone(),
            workload_name: self.workload_name.clone(),
            workload_id: protocol::workload_id(&owner, &self.workload_name, self.replica_index),
            manifest_id: protocol::route_id(&owner, &self.workload_name),
            replica_index: self.replica_index,
            replica_count: self.replica_count,
            challenge: request.challenge,
            endpoint,
            credential_expires_at_secs: credential
                .as_ref()
                .map(|identity| identity.claims.expires_at_secs),
            credential,
            service_record: match &self.network {
                Some(network) => network.record()?,
                None => None,
            },
            transport_signature: String::new(),
        }
        .sign(&self.identity.transport_secret().to_bytes(), now)
    }
}

/// Owns the socket and its cleanup. Refuses symlinks and active listeners; stale
/// sockets are replaced only inside a private directory owned by this process.
pub(crate) struct ConnectorControl {
    listener: UnixListener,
    path: PathBuf,
    inode: u64,
}

impl ConnectorControl {
    pub async fn bind(path: &Path) -> Result<Self> {
        validate_socket_path(path)?;
        let parent = path.parent().context("connector socket has no parent")?;
        create_private_directory(parent)?;
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            ensure!(
                metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
                "connector socket path is not an owned socket"
            );
            match tokio::time::timeout(CONNECTOR_CONTROL_TIMEOUT, UnixStream::connect(path)).await {
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    // Verify that the pathname was not replaced during connect.
                    let current = std::fs::symlink_metadata(path)?;
                    ensure!(
                        current.ino() == metadata.ino() && current.dev() == metadata.dev(),
                        "connector socket changed while binding"
                    );
                    std::fs::remove_file(path)?;
                }
                _ => anyhow::bail!("connector socket is already in use"),
            }
        }
        let listener = UnixListener::bind(path).context("bind private connector socket")?;
        let metadata = std::fs::symlink_metadata(path)?;
        let control = Self {
            listener,
            path: path.to_owned(),
            inode: metadata.ino(),
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(control)
    }

    pub async fn serve(self, context: ConnectorContext, shutdown: CancellationToken) -> Result<()> {
        let slots = Arc::new(Semaphore::new(MAX_CONTROL_CONNECTIONS));
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                    if result.is_err() { log::warn!("connector control handler terminated unexpectedly"); }
                }
                result = self.listener.accept() => {
                    let (mut stream, _) = result.context("accept connector control request")?;
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    // File permissions alone are not relied upon for peer UID.
                    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } { continue; }
                    let context = context.clone();
                    handlers.spawn(async move {
                        let _permit = permit;
                        let result = tokio::time::timeout(CONNECTOR_CONTROL_TIMEOUT, async {
                            let bytes = read_frame(&mut stream).await?;
                            let response = match ConnectorRequest::from_bytes(&bytes) {
                                Ok(request) => {
                                    let installation = if let Some(record) = request.seed_peer.clone() {
                                        context.network.as_ref().context("workload DNS is not enabled").and_then(|network| network.seed(record))
                                    } else if let Some(identity) = request.install_identity.clone() {
                                        match SystemTime::now().duration_since(UNIX_EPOCH) {
                                            Ok(now) => context.authorization.install(identity, now.as_secs()),
                                            Err(error) => Err(error.into()),
                                        }
                                    } else { Ok(()) };
                                    match installation.and_then(|()| context.inspect(&request)) {
                                        Ok(report) => ConnectorResponse::identity(report),
                                        Err(_) => ConnectorResponse::error(ConnectorError::Rejected),
                                    }
                                },
                                Err(_) => ConnectorResponse::error(ConnectorError::Rejected),
                            };
                            write_frame(&mut stream, &response.to_bytes()?).await
                        }).await;
                        if !matches!(result, Ok(Ok(()))) {
                            log::debug!("connector control request failed or exceeded deadline");
                        }
                    });
                }
            }
        }
        handlers.abort_all();
        while handlers.join_next().await.is_some() {}
        Ok(())
    }
}

impl Drop for ConnectorControl {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn validate_socket_path(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        path.is_absolute() && path.as_os_str().as_bytes().len() < 104,
        "connector socket must be a short absolute path"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "invalid connector socket path"
    );
    ensure!(
        path.file_name().is_some(),
        "connector socket filename is missing"
    );
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = path.parent() {
        // Existing ancestors can be conventional /var/run or /tmp directories,
        // but never a workload-supplied symlink redirecting our private state.
        if std::fs::symlink_metadata(path).is_err() {
            if !parent.exists() {
                create_private_directory(parent)?;
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(path)
                .context("create connector control directory")?;
        }
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "connector control parent is not a directory"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "connector control directory must be private and owned"
    );
    Ok(())
}

/// The binary's runtime adapter calls this with one framed request on stdin.
/// Its only output is one framed response, with no logs or credentials on stdout.
pub async fn exchange(path: &Path, request: &ConnectorRequest) -> Result<ConnectorResponse> {
    validate_socket_path(path)?;
    request.validate()?;
    tokio::time::timeout(CONNECTOR_CONTROL_TIMEOUT, async {
        let mut stream = UnixStream::connect(path)
            .await
            .context("connect workload connector")?;
        ensure!(
            stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
            "connector server UID mismatch"
        );
        write_frame(&mut stream, &request.to_bytes()?).await?;
        ConnectorResponse::from_bytes(&read_frame(&mut stream).await?)
    })
    .await
    .context("connector exchange timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn context() -> ConnectorContext {
        let identity = Arc::new(NodeIdentity::ephemeral());
        let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(identity.transport_secret().clone())
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .clear_relay_transports()
            .bind()
            .await
            .unwrap();
        let (owner, owner_private) = crypto::generate_signing_keypair();
        let owner_b64 = crypto::b64_encode(&owner);
        let parent = protocol::workload_identity::EnrollmentDelegation::mint(
            &owner_private,
            protocol::workload_identity::EnrollmentScope {
                tenant_owner: owner_b64.clone(),
                manifest_id: protocol::route_id(&owner, "demo"),
                workload_id: protocol::workload_id(&owner, "demo", 1),
                revision_id: protocol::revision_id(b"demo"),
                issuer_public_key_b64: owner_b64.clone(),
                token_id: "test-generation".into(),
                issued_at_secs: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                expires_at_secs: None,
                max_child_lifetime_secs: protocol::workload_identity::MAX_IDENTITY_LIFETIME_SECS,
            },
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let authorization = Arc::new(
            crate::authorization::WorkloadAuthorization::new(
                &parent.to_b64().unwrap(),
                &owner_b64,
                &parent.scope.manifest_id,
                &parent.scope.workload_id,
                endpoint.id().to_string(),
                identity.signing_public().to_vec(),
            )
            .unwrap(),
        );
        ConnectorContext {
            owner: crypto::b64_encode(&owner),
            workload_name: "demo".into(),
            replica_index: 1,
            replica_count: 3,
            endpoint,
            identity,
            authorization,
            network: None,
        }
    }

    #[tokio::test]
    async fn private_socket_reports_real_identity_and_cleans_up_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/control.sock");
        let context = context().await;
        let endpoint = context.endpoint.clone();
        let owner = context.owner.clone();
        let expected_id = protocol::workload_id(&crypto::b64_decode(&owner).unwrap(), "demo", 1);
        let control = ConnectorControl::bind(&path).await.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(ConnectorControl::bind(&path).await.is_err());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(control.serve(context, shutdown.clone()));
        let response = exchange(&path, &ConnectorRequest::inspect([9; 32]))
            .await
            .unwrap();
        let report = response.identity.unwrap();
        report
            .verify(
                &owner,
                &expected_id,
                &[9; 32],
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap();
        assert_eq!(report.endpoint.endpoint_id, endpoint.id().as_bytes());
        let mut partial = UnixStream::connect(&path).await.unwrap();
        partial.write_u32(100).await.unwrap();
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!path.exists());
        endpoint.close().await;
    }

    #[tokio::test]
    async fn unsafe_paths_and_stale_socket_replacement_are_checked() {
        assert!(validate_socket_path(Path::new("relative.sock")).is_err());
        assert!(validate_socket_path(Path::new("/tmp/../control.sock")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = private.join("control.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(ConnectorControl::bind(&path).await.is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dir.path(), &path).unwrap();
        assert!(ConnectorControl::bind(&path).await.is_err());
        std::fs::remove_file(&path).unwrap();
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        let control = ConnectorControl::bind(&path).await.unwrap();
        drop(control);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn malformed_frames_do_not_stop_other_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/control.sock");
        let context = context().await;
        let endpoint = context.endpoint.clone();
        let control = ConnectorControl::bind(&path).await.unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(control.serve(context, shutdown.clone()));
        let mut stream = UnixStream::connect(&path).await.unwrap();
        write_frame(&mut stream, b"{}").await.unwrap();
        let response =
            ConnectorResponse::from_bytes(&read_frame(&mut stream).await.unwrap()).unwrap();
        assert_eq!(response.error, Some(ConnectorError::Rejected));
        assert!(
            exchange(&path, &ConnectorRequest::inspect([3; 32]))
                .await
                .unwrap()
                .identity
                .is_some()
        );
        shutdown.cancel();
        task.await.unwrap().unwrap();
        endpoint.close().await;
    }
}
