//! Which agents this owner is willing to hand plaintext to.
//!
//! A `CapacityOffer` is self-signed: the key that validates the signature is
//! carried inside the offer, and the offer also names the KEM key every payload
//! is encrypted to. So whoever answers `GET /api/v1/agents/select` chooses the
//! recipient — and therefore chooses who can read the workload. Verifying the
//! offer proves only that it is internally consistent; it proves nothing about
//! *who* the agent is.
//!
//! The owner therefore keeps a list of agent signing keys it is prepared to
//! deploy to. Selection results are checked against that list before any
//! ciphertext is produced, which is what makes the scheduler a blind relay in
//! practice and not just by intent.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use thiserror::Error;

/// File inside the key directory listing one base64 agent signing key per line.
pub const TRUSTED_AGENTS_FILE: &str = "trusted_agents";
/// Comma-separated base64 agent signing keys, overriding the file when set.
pub const TRUSTED_AGENTS_ENV_VAR: &str = "PODMESH_TRUSTED_AGENTS";
/// Bounds the store so a stray file cannot be loaded without limit.
pub const MAX_TRUSTED_AGENTS: usize = 1024;
/// Bounds proxy identities independently from agent identities.
pub const MAX_TRUSTED_PROXIES: usize = 32;
/// Bounds the trust document before parsing.
pub const MAX_TRUST_FILE_BYTES: usize = 256 * 1024;
/// The only typed trust format this release accepts.
pub const TRUST_FILE_HEADER: &str = "podmesh-trust-v1";

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("trust document exceeds the {MAX_TRUST_FILE_BYTES} byte limit")]
    DocumentTooLarge,
    #[error("unsupported trust document version {0:?}")]
    UnsupportedVersion(String),
    #[error("trust record on line {line} is malformed: {reason}")]
    MalformedRecord { line: usize, reason: String },
    #[error("invalid agent signing key {0:?}")]
    InvalidAgentKey(String),
    #[error("invalid proxy signing key {0:?}")]
    InvalidProxySigningKey(String),
    #[error("invalid proxy EndpointId {0:?}")]
    InvalidProxyEndpoint(String),
    #[error("invalid proxy origin {0:?}: {1}")]
    InvalidProxyOrigin(String, String),
    #[error("duplicate proxy origin {0}")]
    DuplicateProxyOrigin(String),
    #[error("more than {MAX_TRUSTED_AGENTS} trusted agent keys configured")]
    AgentLimitExceeded,
    #[error("more than {MAX_TRUSTED_PROXIES} trusted proxy origins configured")]
    ProxyLimitExceeded,
    #[error("proxy {0} is already trusted; use --replace to change its identity")]
    ExistingProxyTrust(String),
    #[error("proxy {0} is not trusted, so it cannot be replaced")]
    MissingProxyForReplace(String),
    #[error("trust store {operation} failed for {}: {detail}", path.display())]
    StoreIo {
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },
    #[error(
        "trust store persistence failed during {operation}: {detail}; reload trust before retrying"
    )]
    Persistence {
        operation: &'static str,
        detail: String,
    },
    #[error("proxy {0} is not trusted; run `podctl cert trust-proxy --proxy-url {0}` first")]
    MissingProxyTrust(String),
    #[error(
        "proxy {origin} identity changed (trusted endpoint {trusted_endpoint}, observed endpoint {observed_endpoint}; trusted signing key {trusted_signing_key}, observed signing key {observed_signing_key}); inspect it and run `podctl cert trust-proxy --proxy-url {origin} --replace`"
    )]
    ProxyIdentityMismatch {
        origin: String,
        trusted_endpoint: String,
        observed_endpoint: String,
        trusted_signing_key: String,
        observed_signing_key: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CanonicalProxyOrigin(String);

impl CanonicalProxyOrigin {
    pub fn parse(input: &str) -> std::result::Result<Self, TrustError> {
        let parsed = reqwest::Url::parse(input).map_err(|error| {
            TrustError::InvalidProxyOrigin(input.to_string(), error.to_string())
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(TrustError::InvalidProxyOrigin(
                input.to_string(),
                "scheme must be http or https".to_string(),
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(TrustError::InvalidProxyOrigin(
                input.to_string(),
                "credentials are not permitted".to_string(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(TrustError::InvalidProxyOrigin(
                input.to_string(),
                "query strings and fragments are not permitted".to_string(),
            ));
        }
        if !matches!(parsed.path(), "" | "/") {
            return Err(TrustError::InvalidProxyOrigin(
                input.to_string(),
                "path prefixes are not permitted".to_string(),
            ));
        }
        if parsed.host().is_none() {
            return Err(TrustError::InvalidProxyOrigin(
                input.to_string(),
                "host is required".to_string(),
            ));
        }
        Ok(Self(parsed.origin().ascii_serialization()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CanonicalProxyOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedProxyIdentity {
    pub origin: CanonicalProxyOrigin,
    pub endpoint_id_hex: String,
    pub signing_key_b64: String,
}

impl TrustedProxyIdentity {
    pub fn new(
        origin: CanonicalProxyOrigin,
        endpoint_id_hex: &str,
        signing_key_b64: &str,
    ) -> std::result::Result<Self, TrustError> {
        let endpoint_id = hex::decode(endpoint_id_hex)
            .map_err(|_| TrustError::InvalidProxyEndpoint(endpoint_id_hex.to_string()))?;
        if endpoint_id.len() != protocol::IROH_ENDPOINT_ID_BYTES
            || endpoint_id_hex != endpoint_id_hex.to_ascii_lowercase()
        {
            return Err(TrustError::InvalidProxyEndpoint(
                endpoint_id_hex.to_string(),
            ));
        }
        validate_signing_key(signing_key_b64, false)?;
        Ok(Self {
            origin,
            endpoint_id_hex: endpoint_id_hex.to_string(),
            signing_key_b64: signing_key_b64.to_string(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustRegistry {
    agent_keys: BTreeSet<String>,
    proxies: BTreeMap<CanonicalProxyOrigin, TrustedProxyIdentity>,
}

impl TrustRegistry {
    pub fn parse(contents: &str) -> std::result::Result<Self, TrustError> {
        if contents.len() > MAX_TRUST_FILE_BYTES {
            return Err(TrustError::DocumentTooLarge);
        }
        let first_meaningful = contents
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'));
        if first_meaningful.is_some_and(|line| line.starts_with("podmesh-trust-")) {
            Self::parse_typed(contents)
        } else {
            Self::parse_legacy(contents)
        }
    }

    fn parse_legacy(contents: &str) -> std::result::Result<Self, TrustError> {
        let mut registry = Self::default();
        for value in contents
            .lines()
            .map(|line| line.split('#').next().unwrap_or_default().trim())
        {
            if value.is_empty() {
                continue;
            }
            registry.insert_agent(value)?;
        }
        Ok(registry)
    }

    fn parse_typed(contents: &str) -> std::result::Result<Self, TrustError> {
        let mut lines = contents.lines().enumerate();
        let mut header_seen = false;
        let mut registry = Self::default();
        for (index, raw) in lines.by_ref() {
            let line_number = index + 1;
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if !header_seen {
                if line != TRUST_FILE_HEADER {
                    return Err(TrustError::UnsupportedVersion(line.to_string()));
                }
                header_seen = true;
                continue;
            }
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            match fields.as_slice() {
                ["agent", key] => registry.insert_agent(key)?,
                ["proxy", origin, endpoint, key] => {
                    let origin = CanonicalProxyOrigin::parse(origin)?;
                    if registry.proxies.contains_key(&origin) {
                        return Err(TrustError::DuplicateProxyOrigin(origin.to_string()));
                    }
                    if registry.proxies.len() >= MAX_TRUSTED_PROXIES {
                        return Err(TrustError::ProxyLimitExceeded);
                    }
                    let identity = TrustedProxyIdentity::new(origin.clone(), endpoint, key)?;
                    registry.proxies.insert(origin, identity);
                }
                _ => {
                    return Err(TrustError::MalformedRecord {
                        line: line_number,
                        reason: "expected `agent <key>` or `proxy <origin> <endpoint> <key>`"
                            .to_string(),
                    });
                }
            }
        }
        if !header_seen {
            return Err(TrustError::UnsupportedVersion(String::new()));
        }
        Ok(registry)
    }

    fn insert_agent(&mut self, key: &str) -> std::result::Result<(), TrustError> {
        validate_signing_key(key, true)?;
        if !self.agent_keys.contains(key) && self.agent_keys.len() >= MAX_TRUSTED_AGENTS {
            return Err(TrustError::AgentLimitExceeded);
        }
        self.agent_keys.insert(key.to_string());
        Ok(())
    }

    pub fn to_canonical_string(&self) -> String {
        let mut output = String::from(TRUST_FILE_HEADER);
        output.push('\n');
        for key in &self.agent_keys {
            output.push_str("agent ");
            output.push_str(key);
            output.push('\n');
        }
        for identity in self.proxies.values() {
            output.push_str("proxy ");
            output.push_str(identity.origin.as_str());
            output.push(' ');
            output.push_str(&identity.endpoint_id_hex);
            output.push(' ');
            output.push_str(&identity.signing_key_b64);
            output.push('\n');
        }
        output
    }

    pub fn agent_keys(&self) -> &BTreeSet<String> {
        &self.agent_keys
    }

    pub fn effective_agent_keys(
        &self,
        environment_override: Option<&str>,
    ) -> std::result::Result<BTreeSet<String>, TrustError> {
        let Some(value) = environment_override else {
            return Ok(self.agent_keys.clone());
        };
        let mut keys = BTreeSet::new();
        for key in value
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            validate_signing_key(key, true)?;
            if !keys.contains(key) && keys.len() >= MAX_TRUSTED_AGENTS {
                return Err(TrustError::AgentLimitExceeded);
            }
            keys.insert(key.to_string());
        }
        Ok(keys)
    }

    pub fn proxies(&self) -> &BTreeMap<CanonicalProxyOrigin, TrustedProxyIdentity> {
        &self.proxies
    }

    pub fn insert_proxy(
        &mut self,
        identity: TrustedProxyIdentity,
    ) -> std::result::Result<(), TrustError> {
        if self.proxies.contains_key(&identity.origin) {
            return Err(TrustError::ExistingProxyTrust(identity.origin.to_string()));
        }
        if self.proxies.len() >= MAX_TRUSTED_PROXIES {
            return Err(TrustError::ProxyLimitExceeded);
        }
        self.proxies.insert(identity.origin.clone(), identity);
        Ok(())
    }

    pub fn replace_proxy(
        &mut self,
        identity: TrustedProxyIdentity,
    ) -> std::result::Result<TrustedProxyIdentity, TrustError> {
        if !self.proxies.contains_key(&identity.origin) {
            return Err(TrustError::MissingProxyForReplace(
                identity.origin.to_string(),
            ));
        }
        self.proxies
            .insert(identity.origin.clone(), identity)
            .ok_or_else(|| TrustError::MissingProxyForReplace("unknown proxy".to_string()))
    }

    pub fn remove_proxy(&mut self, origin: &CanonicalProxyOrigin) -> Option<TrustedProxyIdentity> {
        self.proxies.remove(origin)
    }

    pub fn authorize_proxy(
        &self,
        observed: &TrustedProxyIdentity,
    ) -> std::result::Result<&TrustedProxyIdentity, TrustError> {
        let trusted = self
            .proxies
            .get(&observed.origin)
            .ok_or_else(|| TrustError::MissingProxyTrust(observed.origin.to_string()))?;
        if trusted.endpoint_id_hex != observed.endpoint_id_hex
            || trusted.signing_key_b64 != observed.signing_key_b64
        {
            return Err(TrustError::ProxyIdentityMismatch {
                origin: observed.origin.to_string(),
                trusted_endpoint: short_identifier(&trusted.endpoint_id_hex),
                observed_endpoint: short_identifier(&observed.endpoint_id_hex),
                trusted_signing_key: short_identifier(&trusted.signing_key_b64),
                observed_signing_key: short_identifier(&observed.signing_key_b64),
            });
        }
        Ok(trusted)
    }
}

pub fn normalize_proxy_url(input: &str) -> std::result::Result<String, TrustError> {
    CanonicalProxyOrigin::parse(input).map(|origin| origin.to_string())
}

#[derive(Debug, Clone, Copy)]
struct TrustMetadata {
    exists: bool,
    len: u64,
    mode: Option<u32>,
}

trait TrustStoreIo {
    type Temp;

    fn metadata(&self, path: &Path) -> std::result::Result<TrustMetadata, TrustError>;
    fn set_private_permissions(&self, path: &Path) -> std::result::Result<(), TrustError>;
    fn read_bounded(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> std::result::Result<Vec<u8>, TrustError>;
    fn create_private_temp(
        &self,
        destination: &Path,
    ) -> std::result::Result<Self::Temp, TrustError>;
    fn write_all(&self, temp: &mut Self::Temp, bytes: &[u8])
    -> std::result::Result<(), TrustError>;
    fn sync_file(&self, temp: &Self::Temp) -> std::result::Result<(), TrustError>;
    fn rename(&self, temp: Self::Temp, destination: &Path) -> std::result::Result<(), TrustError>;
    fn sync_parent(&self, destination: &Path) -> std::result::Result<(), TrustError>;
}

struct OsTrustStoreIo;

struct OsTempTrustFile {
    file: std::fs::File,
    path: PathBuf,
}

impl Drop for OsTempTrustFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn store_io_error(
    operation: &'static str,
    path: &Path,
    error: impl std::fmt::Display,
) -> TrustError {
    TrustError::StoreIo {
        operation,
        path: path.to_path_buf(),
        detail: error.to_string(),
    }
}

fn persistence_error(operation: &'static str, error: impl std::fmt::Display) -> TrustError {
    TrustError::Persistence {
        operation,
        detail: error.to_string(),
    }
}

impl TrustStoreIo for OsTrustStoreIo {
    type Temp = OsTempTrustFile;

    fn metadata(&self, path: &Path) -> std::result::Result<TrustMetadata, TrustError> {
        match std::fs::metadata(path) {
            Ok(metadata) => {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    Some(metadata.permissions().mode() & 0o777)
                };
                #[cfg(not(unix))]
                let mode = None;
                Ok(TrustMetadata {
                    exists: true,
                    len: metadata.len(),
                    mode,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TrustMetadata {
                exists: false,
                len: 0,
                mode: None,
            }),
            Err(error) => Err(store_io_error("metadata", path, error)),
        }
    }

    fn set_private_permissions(&self, path: &Path) -> std::result::Result<(), TrustError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| store_io_error("restrict permissions", path, error))?;
        }
        Ok(())
    }

    fn read_bounded(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> std::result::Result<Vec<u8>, TrustError> {
        let file =
            std::fs::File::open(path).map_err(|error| store_io_error("open", path, error))?;
        let limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(|error| store_io_error("read", path, error))?;
        if bytes.len() > max_bytes {
            return Err(TrustError::DocumentTooLarge);
        }
        Ok(bytes)
    }

    fn create_private_temp(
        &self,
        destination: &Path,
    ) -> std::result::Result<Self::Temp, TrustError> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        for _ in 0..16 {
            let path =
                destination.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().as_simple()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path) {
                Ok(file) => return Ok(OsTempTrustFile { file, path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(persistence_error("create temporary file", error)),
            }
        }
        Err(persistence_error(
            "create temporary file",
            "temporary filename collision limit reached",
        ))
    }

    fn write_all(
        &self,
        temp: &mut Self::Temp,
        bytes: &[u8],
    ) -> std::result::Result<(), TrustError> {
        temp.file
            .write_all(bytes)
            .map_err(|error| persistence_error("write temporary file", error))?;
        temp.file
            .flush()
            .map_err(|error| persistence_error("flush temporary file", error))
    }

    fn sync_file(&self, temp: &Self::Temp) -> std::result::Result<(), TrustError> {
        temp.file
            .sync_all()
            .map_err(|error| persistence_error("sync temporary file", error))
    }

    fn rename(&self, temp: Self::Temp, destination: &Path) -> std::result::Result<(), TrustError> {
        std::fs::rename(&temp.path, destination)
            .map_err(|error| persistence_error("rename trust file", error))
    }

    fn sync_parent(&self, destination: &Path) -> std::result::Result<(), TrustError> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let directory = std::fs::File::open(parent)
            .map_err(|error| persistence_error("open trust directory", error))?;
        directory
            .sync_all()
            .map_err(|error| persistence_error("sync trust directory", error))
    }
}

fn load_registry_with_io<I: TrustStoreIo>(
    io: &I,
    path: &Path,
) -> std::result::Result<TrustRegistry, TrustError> {
    let metadata = io.metadata(path)?;
    if !metadata.exists {
        return Ok(TrustRegistry::default());
    }
    if metadata.len > MAX_TRUST_FILE_BYTES as u64 {
        return Err(TrustError::DocumentTooLarge);
    }
    if metadata.mode.is_some_and(|mode| mode & 0o077 != 0) {
        io.set_private_permissions(path)?;
    }
    let bytes = io.read_bounded(path, MAX_TRUST_FILE_BYTES)?;
    let contents = std::str::from_utf8(&bytes).map_err(|error| TrustError::MalformedRecord {
        line: 0,
        reason: format!("trust document is not UTF-8: {error}"),
    })?;
    TrustRegistry::parse(contents)
}

fn persist_registry_with_io<I: TrustStoreIo>(
    io: &I,
    path: &Path,
    registry: &TrustRegistry,
) -> std::result::Result<(), TrustError> {
    let contents = registry.to_canonical_string();
    if contents.len() > MAX_TRUST_FILE_BYTES {
        return Err(TrustError::DocumentTooLarge);
    }
    let mut temp = io.create_private_temp(path)?;
    io.write_all(&mut temp, contents.as_bytes())?;
    io.sync_file(&temp)?;
    io.rename(temp, path)?;
    io.sync_parent(path)
}

pub fn load_registry(key_dir: &Path) -> std::result::Result<TrustRegistry, TrustError> {
    load_registry_with_io(&OsTrustStoreIo, &AgentTrust::path(key_dir))
}

pub fn store_proxy_trust(
    key_dir: &Path,
    identity: TrustedProxyIdentity,
    replace: bool,
) -> std::result::Result<Option<TrustedProxyIdentity>, TrustError> {
    let path = AgentTrust::path(key_dir);
    let mut registry = load_registry_with_io(&OsTrustStoreIo, &path)?;
    let previous = if replace {
        Some(registry.replace_proxy(identity)?)
    } else {
        registry.insert_proxy(identity)?;
        None
    };
    persist_registry_with_io(&OsTrustStoreIo, &path, &registry)?;
    Ok(previous)
}

pub fn remove_proxy_trust(
    key_dir: &Path,
    proxy_url: &str,
) -> std::result::Result<Option<TrustedProxyIdentity>, TrustError> {
    let path = AgentTrust::path(key_dir);
    let mut registry = load_registry_with_io(&OsTrustStoreIo, &path)?;
    let origin = CanonicalProxyOrigin::parse(proxy_url)?;
    let removed = registry.remove_proxy(&origin);
    if removed.is_some() {
        persist_registry_with_io(&OsTrustStoreIo, &path, &registry)?;
    }
    Ok(removed)
}

pub fn list_proxy_trust(
    key_dir: &Path,
) -> std::result::Result<Vec<TrustedProxyIdentity>, TrustError> {
    Ok(load_registry(key_dir)?.proxies.values().cloned().collect())
}

fn validate_signing_key(value: &str, agent: bool) -> std::result::Result<(), TrustError> {
    let decoded = crypto::b64_decode(value).map_err(|_| {
        if agent {
            TrustError::InvalidAgentKey(value.to_string())
        } else {
            TrustError::InvalidProxySigningKey(value.to_string())
        }
    })?;
    if decoded.len() != crypto::ED25519_PUBLIC_KEY_SIZE || crypto::b64_encode(&decoded) != value {
        return Err(if agent {
            TrustError::InvalidAgentKey(value.to_string())
        } else {
            TrustError::InvalidProxySigningKey(value.to_string())
        });
    }
    Ok(())
}

fn short_identifier(value: &str) -> String {
    value.chars().take(16).collect()
}

/// The owner's decision about which agents may host its workloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTrust {
    /// Only these base64 signing keys are acceptable.
    Allowlist(BTreeSet<String>),
    /// Any agent the mesh offers is acceptable. Chosen deliberately by the
    /// operator; it means any scheduler can nominate itself as the recipient.
    AnyAgent,
}

impl AgentTrust {
    /// Load the trust store, or return `None` when the owner has not made a
    /// decision yet. Callers must not silently treat `None` as "trust anything".
    pub fn load(key_dir: &Path) -> Result<Option<Self>> {
        if let Ok(value) = std::env::var(TRUSTED_AGENTS_ENV_VAR) {
            let keys = parse_keys(value.split(','))?;
            return Ok(if keys.is_empty() {
                None
            } else {
                Some(Self::Allowlist(keys))
            });
        }
        let registry = load_registry(key_dir)
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!("load trusted agent list {}", Self::path(key_dir).display())
            })?;
        let keys = registry.agent_keys;
        Ok(if keys.is_empty() {
            None
        } else {
            Some(Self::Allowlist(keys))
        })
    }

    pub fn path(key_dir: &Path) -> PathBuf {
        key_dir.join(TRUSTED_AGENTS_FILE)
    }

    /// Refuse an agent this owner has not agreed to trust.
    pub fn authorize(&self, agent_signing_pubkey: &str) -> Result<()> {
        match self {
            Self::AnyAgent => Ok(()),
            Self::Allowlist(keys) => {
                ensure!(
                    keys.contains(agent_signing_pubkey),
                    "the mesh offered agent {agent_signing_pubkey}, which is not in \
                     {TRUSTED_AGENTS_FILE}; add its signing key or re-run with --trust-any-agent"
                );
                Ok(())
            }
        }
    }
}

/// Resolve the trust decision for one invocation.
///
/// When no allowlist exists the operator must say so explicitly. Defaulting to
/// "trust anything" would silently hand the scheduler the ability to read every
/// workload, which is exactly the property the design claims to deny it.
pub fn resolve(key_dir: &Path, trust_any_agent: bool) -> Result<AgentTrust> {
    match (AgentTrust::load(key_dir)?, trust_any_agent) {
        (Some(AgentTrust::Allowlist(keys)), false) => Ok(AgentTrust::Allowlist(keys)),
        (_, true) => {
            log::warn!(
                "--trust-any-agent: any agent the mesh offers may read this workload's plaintext"
            );
            Ok(AgentTrust::AnyAgent)
        }
        (Some(AgentTrust::AnyAgent), false) => Ok(AgentTrust::AnyAgent),
        (None, false) => bail!(
            "no trusted agents configured. List the base64 Ed25519 signing keys of the \
             agents you are willing to run workloads on in {}, set {TRUSTED_AGENTS_ENV_VAR}, \
             or pass --trust-any-agent to accept whichever agent the mesh offers",
            AgentTrust::path(key_dir).display()
        ),
    }
}

fn parse_keys<'a>(values: impl Iterator<Item = &'a str>) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        validate_signing_key(value, true).map_err(anyhow::Error::from)?;
        ensure!(
            keys.len() < MAX_TRUSTED_AGENTS,
            "more than {MAX_TRUSTED_AGENTS} trusted agent keys configured"
        );
        keys.insert(value.to_string());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn key(byte: u8) -> String {
        crypto::b64_encode(&[byte; crypto::ED25519_PUBLIC_KEY_SIZE])
    }

    #[test]
    fn an_allowlist_admits_only_listed_agents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            AgentTrust::path(dir.path()),
            format!("{}  # lab agent\n\n{}\n", key(1), key(2)),
        )
        .unwrap();
        let trust = AgentTrust::load(dir.path()).unwrap().unwrap();
        trust.authorize(&key(1)).unwrap();
        trust.authorize(&key(2)).unwrap();
        assert!(trust.authorize(&key(3)).is_err());
    }

    #[test]
    fn missing_trust_store_is_an_error_unless_the_operator_opts_out() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve(dir.path(), false).unwrap_err();
        assert!(error.to_string().contains("no trusted agents configured"));
        assert_eq!(resolve(dir.path(), true).unwrap(), AgentTrust::AnyAgent);
    }

    #[test]
    fn malformed_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(AgentTrust::path(dir.path()), "not-base64!!\n").unwrap();
        assert!(AgentTrust::load(dir.path()).is_err());

        std::fs::write(AgentTrust::path(dir.path()), crypto::b64_encode(b"short")).unwrap();
        assert!(AgentTrust::load(dir.path()).is_err());
    }

    fn proxy(origin: &str, endpoint: u8, signing_key: u8) -> String {
        format!(
            "proxy {origin} {} {}",
            hex::encode([endpoint; protocol::IROH_ENDPOINT_ID_BYTES]),
            key(signing_key)
        )
    }

    #[test]
    fn legacy_comments_and_duplicate_keys_are_normalized() {
        let contents = format!("{} # first\n\n{}\n{} # duplicate\n", key(2), key(1), key(2));
        let registry = TrustRegistry::parse(&contents).unwrap();
        assert_eq!(registry.agent_keys().len(), 2);
        assert_eq!(
            registry.to_canonical_string(),
            format!("{TRUST_FILE_HEADER}\nagent {}\nagent {}\n", key(1), key(2))
        );
    }

    #[test]
    fn typed_registry_round_trips_in_canonical_order() {
        let contents = format!(
            "{TRUST_FILE_HEADER}\n{}\nagent {}\n{}\n",
            proxy("https://z.example", 2, 3),
            key(4),
            proxy("http://a.example:7100", 1, 2),
        );
        let registry = TrustRegistry::parse(&contents).unwrap();
        let canonical = registry.to_canonical_string();
        assert_eq!(TrustRegistry::parse(&canonical).unwrap(), registry);
        assert!(
            canonical.find("http://a.example:7100").unwrap()
                < canonical.find("https://z.example").unwrap()
        );
    }

    #[test]
    fn unsupported_typed_versions_and_duplicate_proxies_are_refused() {
        assert!(matches!(
            TrustRegistry::parse("podmesh-trust-v2\n"),
            Err(TrustError::UnsupportedVersion(_))
        ));
        let duplicate = format!(
            "{TRUST_FILE_HEADER}\n{}\n{}\n",
            proxy("http://proxy.example", 1, 1),
            proxy("http://proxy.example/", 2, 2),
        );
        assert!(matches!(
            TrustRegistry::parse(&duplicate),
            Err(TrustError::DuplicateProxyOrigin(_))
        ));
    }

    #[test]
    fn proxy_origins_are_structurally_normalized() {
        assert_eq!(
            normalize_proxy_url("HTTP://Example.COM:80/").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            normalize_proxy_url("https://[::1]:7443/").unwrap(),
            "https://[::1]:7443"
        );
        for invalid in [
            "ftp://example.com",
            "http://user@example.com",
            "http://example.com/path",
            "http://example.com?query=1",
            "http://example.com/#fragment",
        ] {
            assert!(normalize_proxy_url(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn proxy_authorization_requires_every_identity_field() {
        let contents = format!(
            "{TRUST_FILE_HEADER}\n{}\n",
            proxy("http://proxy.example", 1, 2)
        );
        let registry = TrustRegistry::parse(&contents).unwrap();
        let origin = CanonicalProxyOrigin::parse("http://proxy.example/").unwrap();
        let accepted = TrustedProxyIdentity::new(
            origin.clone(),
            &hex::encode([1; protocol::IROH_ENDPOINT_ID_BYTES]),
            &key(2),
        )
        .unwrap();
        assert!(registry.authorize_proxy(&accepted).is_ok());
        let substituted = TrustedProxyIdentity::new(
            origin,
            &hex::encode([3; protocol::IROH_ENDPOINT_ID_BYTES]),
            &key(2),
        )
        .unwrap();
        assert!(matches!(
            registry.authorize_proxy(&substituted),
            Err(TrustError::ProxyIdentityMismatch { .. })
        ));
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailurePoint {
        Metadata,
        Permissions,
        Read,
        Create,
        Write,
        SyncFile,
        Rename,
        SyncParent,
    }

    #[derive(Default)]
    struct MemoryTrustStoreIo {
        bytes: RefCell<Option<Vec<u8>>>,
        mode: Cell<u32>,
        fail: Cell<Option<FailurePoint>>,
    }

    impl MemoryTrustStoreIo {
        fn with_contents(contents: &str) -> Self {
            Self {
                bytes: RefCell::new(Some(contents.as_bytes().to_vec())),
                mode: Cell::new(0o600),
                fail: Cell::new(None),
            }
        }

        fn fail_at(&self, point: FailurePoint) {
            self.fail.set(Some(point));
        }

        fn check(&self, point: FailurePoint) -> std::result::Result<(), TrustError> {
            if self.fail.get() == Some(point) {
                return Err(persistence_error("injected fault", format!("{point:?}")));
            }
            Ok(())
        }
    }

    impl TrustStoreIo for MemoryTrustStoreIo {
        type Temp = Vec<u8>;

        fn metadata(&self, _path: &Path) -> std::result::Result<TrustMetadata, TrustError> {
            self.check(FailurePoint::Metadata)?;
            let bytes = self.bytes.borrow();
            Ok(TrustMetadata {
                exists: bytes.is_some(),
                len: bytes.as_ref().map_or(0, |value| value.len() as u64),
                mode: Some(self.mode.get()),
            })
        }

        fn set_private_permissions(&self, _path: &Path) -> std::result::Result<(), TrustError> {
            self.check(FailurePoint::Permissions)?;
            self.mode.set(0o600);
            Ok(())
        }

        fn read_bounded(
            &self,
            _path: &Path,
            max_bytes: usize,
        ) -> std::result::Result<Vec<u8>, TrustError> {
            self.check(FailurePoint::Read)?;
            let bytes = self.bytes.borrow().clone().unwrap_or_default();
            if bytes.len() > max_bytes {
                return Err(TrustError::DocumentTooLarge);
            }
            Ok(bytes)
        }

        fn create_private_temp(
            &self,
            _destination: &Path,
        ) -> std::result::Result<Self::Temp, TrustError> {
            self.check(FailurePoint::Create)?;
            Ok(Vec::new())
        }

        fn write_all(
            &self,
            temp: &mut Self::Temp,
            bytes: &[u8],
        ) -> std::result::Result<(), TrustError> {
            self.check(FailurePoint::Write)?;
            temp.extend_from_slice(bytes);
            Ok(())
        }

        fn sync_file(&self, _temp: &Self::Temp) -> std::result::Result<(), TrustError> {
            self.check(FailurePoint::SyncFile)
        }

        fn rename(
            &self,
            temp: Self::Temp,
            _destination: &Path,
        ) -> std::result::Result<(), TrustError> {
            self.check(FailurePoint::Rename)?;
            *self.bytes.borrow_mut() = Some(temp);
            Ok(())
        }

        fn sync_parent(&self, _destination: &Path) -> std::result::Result<(), TrustError> {
            self.check(FailurePoint::SyncParent)
        }
    }

    #[test]
    fn proxy_mutations_insert_replace_and_remove_idempotently() {
        let origin = CanonicalProxyOrigin::parse("http://proxy.example").unwrap();
        let first = TrustedProxyIdentity::new(
            origin.clone(),
            &hex::encode([1; protocol::IROH_ENDPOINT_ID_BYTES]),
            &key(2),
        )
        .unwrap();
        let second = TrustedProxyIdentity::new(
            origin.clone(),
            &hex::encode([3; protocol::IROH_ENDPOINT_ID_BYTES]),
            &key(4),
        )
        .unwrap();
        let mut registry = TrustRegistry::default();
        registry.insert_proxy(first.clone()).unwrap();
        assert!(matches!(
            registry.insert_proxy(first.clone()),
            Err(TrustError::ExistingProxyTrust(_))
        ));
        assert_eq!(registry.replace_proxy(second.clone()).unwrap(), first);
        assert_eq!(registry.remove_proxy(&origin), Some(second));
        assert_eq!(registry.remove_proxy(&origin), None);
    }

    #[test]
    fn broad_permissions_are_narrowed_before_reading() {
        let io = MemoryTrustStoreIo::with_contents(&key(1));
        io.mode.set(0o644);
        let registry = load_registry_with_io(&io, Path::new("trusted_agents")).unwrap();
        assert_eq!(registry.agent_keys().len(), 1);
        assert_eq!(io.mode.get(), 0o600);
    }

    #[test]
    fn persistence_failures_never_publish_partial_state() {
        let old = format!("{TRUST_FILE_HEADER}\nagent {}\n", key(1));
        let mut registry = TrustRegistry::parse(&old).unwrap();
        registry
            .insert_proxy(
                TrustedProxyIdentity::new(
                    CanonicalProxyOrigin::parse("http://proxy.example").unwrap(),
                    &hex::encode([2; protocol::IROH_ENDPOINT_ID_BYTES]),
                    &key(3),
                )
                .unwrap(),
            )
            .unwrap();
        let new = registry.to_canonical_string();
        for point in [
            FailurePoint::Create,
            FailurePoint::Write,
            FailurePoint::SyncFile,
            FailurePoint::Rename,
        ] {
            let io = MemoryTrustStoreIo::with_contents(&old);
            io.fail_at(point);
            assert!(persist_registry_with_io(&io, Path::new("trusted_agents"), &registry).is_err());
            assert_eq!(io.bytes.borrow().as_deref(), Some(old.as_bytes()));
        }
        let io = MemoryTrustStoreIo::with_contents(&old);
        io.fail_at(FailurePoint::SyncParent);
        assert!(persist_registry_with_io(&io, Path::new("trusted_agents"), &registry).is_err());
        assert_eq!(io.bytes.borrow().as_deref(), Some(new.as_bytes()));
    }

    #[cfg(unix)]
    #[test]
    fn real_persistence_is_private_and_durable_to_reload() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let identity = TrustedProxyIdentity::new(
            CanonicalProxyOrigin::parse("http://proxy.example").unwrap(),
            &hex::encode([5; protocol::IROH_ENDPOINT_ID_BYTES]),
            &key(6),
        )
        .unwrap();
        store_proxy_trust(dir.path(), identity.clone(), false).unwrap();
        assert_eq!(list_proxy_trust(dir.path()).unwrap(), vec![identity]);
        assert_eq!(
            std::fs::metadata(AgentTrust::path(dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
