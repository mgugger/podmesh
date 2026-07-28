use anyhow::{Context, Result, ensure};
use biscuit_auth::{
    Algorithm, AuthorizerBuilder, Biscuit, KeyPair, PublicKey,
    builder::{Term, fact, string},
};

pub const MAX_BISCUIT_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_AUTHZ_VALUE_LEN: usize = 256;
pub const MAX_INGRESS_PREFIXES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadOperation {
    MeshJoin,
    Registration,
    Ingress,
    Egress,
    ProxyDiscovery,
}

impl WorkloadOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::MeshJoin => "mesh_join",
            Self::Registration => "register",
            Self::Ingress => "ingress",
            Self::Egress => "egress",
            Self::ProxyDiscovery => "proxy_discovery",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkloadCapabilityClaims {
    pub tenant_owner: String,
    pub workload_id: String,
    pub subject_endpoint: String,
    pub audience_endpoint: String,
    pub role: String,
    pub operation: WorkloadOperation,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub token_id: String,
    pub ingress_path_prefixes: Vec<String>,
    pub egress_host: Option<String>,
    pub egress_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct WorkloadAuthorizationContext {
    pub tenant_owner: String,
    pub workload_id: String,
    pub subject_endpoint: String,
    pub audience_endpoint: String,
    pub role: String,
    pub operation: WorkloadOperation,
    pub now_secs: u64,
    pub ingress_path: Option<String>,
    pub egress_host: Option<String>,
    pub egress_port: Option<u16>,
}

pub fn biscuit_keypair_from_ed25519(private_key: &[u8]) -> Result<KeyPair> {
    KeyPair::from_bytes(private_key, Algorithm::Ed25519.into())
        .context("decode Biscuit Ed25519 private key")
}

pub fn biscuit_public_key_from_ed25519(public_key: &[u8]) -> Result<PublicKey> {
    PublicKey::from_bytes(public_key, Algorithm::Ed25519)
        .context("decode Biscuit Ed25519 public key")
}

pub fn mint_workload_biscuit(root: &KeyPair, claims: &WorkloadCapabilityClaims) -> Result<Vec<u8>> {
    claims.validate()?;
    let mut builder = Biscuit::builder()
        .fact(fact("tenant_owner", &[string(&claims.tenant_owner)]))?
        .fact(fact("workload", &[string(&claims.workload_id)]))?
        .fact(fact("subject_peer", &[string(&claims.subject_endpoint)]))?
        .fact(fact("audience_peer", &[string(&claims.audience_endpoint)]))?
        .fact(fact("role", &[string(&claims.role)]))?
        .fact(fact("operation", &[string(claims.operation.as_str())]))?
        .fact(fact(
            "issued_at",
            &[int_term(to_i64(claims.issued_at_secs)?)],
        ))?
        .fact(fact(
            "expires_at",
            &[int_term(to_i64(claims.expires_at_secs)?)],
        ))?
        .fact(fact("token_id", &[string(&claims.token_id)]))?;
    for prefix in &claims.ingress_path_prefixes {
        builder = builder.fact(fact("allowed_path_prefix", &[string(prefix)]))?;
    }
    if let Some(host) = &claims.egress_host {
        builder = builder.fact(fact("allowed_egress_host", &[string(host)]))?;
    }
    if let Some(port) = claims.egress_port {
        builder = builder.fact(fact("allowed_egress_port", &[int_term(i64::from(port))]))?;
    }
    let token = builder.build(root).context("build workload Biscuit")?;
    let encoded = token.to_vec().context("serialize workload Biscuit")?;
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_BISCUIT_TOKEN_BYTES,
        "workload Biscuit encoded size is invalid"
    );
    Ok(encoded)
}

pub fn authorize_workload_biscuit(
    encoded: &[u8],
    root_public: PublicKey,
    context: &WorkloadAuthorizationContext,
) -> Result<()> {
    context.validate()?;
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_BISCUIT_TOKEN_BYTES,
        "workload Biscuit encoded size is invalid"
    );
    let token = Biscuit::from(encoded, root_public).context("verify workload Biscuit signature")?;
    let mut builder = AuthorizerBuilder::new()
        .fact(fact(
            "request_tenant_owner",
            &[string(&context.tenant_owner)],
        ))?
        .fact(fact("request_workload", &[string(&context.workload_id)]))?
        .fact(fact(
            "request_subject_peer",
            &[string(&context.subject_endpoint)],
        ))?
        .fact(fact(
            "request_audience_peer",
            &[string(&context.audience_endpoint)],
        ))?
        .fact(fact("request_role", &[string(&context.role)]))?
        .fact(fact(
            "request_operation",
            &[string(context.operation.as_str())],
        ))?
        .fact(fact("request_time", &[int_term(to_i64(context.now_secs)?)]))?;
    if let Some(path) = &context.ingress_path {
        builder = builder.fact(fact("request_path", &[string(path)]))?;
    }
    if let Some(host) = &context.egress_host {
        builder = builder.fact(fact("request_egress_host", &[string(host)]))?;
    }
    if let Some(port) = context.egress_port {
        builder = builder.fact(fact("request_egress_port", &[int_term(i64::from(port))]))?;
    }
    let policy = match context.operation {
        WorkloadOperation::Ingress => INGRESS_POLICY,
        WorkloadOperation::Egress => EGRESS_POLICY,
        _ => BASE_POLICY,
    };
    let mut authorizer = builder.code(policy)?.build(&token)?;
    authorizer
        .authorize()
        .context("workload Biscuit authorization failed")?;
    Ok(())
}

const BASE_POLICY: &str = r#"
allow if tenant_owner($tenant), request_tenant_owner($tenant),
  workload($workload), request_workload($workload),
  subject_peer($subject), request_subject_peer($subject),
  audience_peer($audience), request_audience_peer($audience),
  role($role), request_role($role),
  operation($operation), request_operation($operation),
  issued_at($issued), expires_at($expires), request_time($now),
  $issued <= $now, $now <= $expires,
  token_id($token_id);
deny if true;
"#;

const INGRESS_POLICY: &str = r#"
allow if tenant_owner($tenant), request_tenant_owner($tenant),
  workload($workload), request_workload($workload),
  subject_peer($subject), request_subject_peer($subject),
  audience_peer($audience), request_audience_peer($audience),
  role($role), request_role($role),
  operation("ingress"), request_operation("ingress"),
  issued_at($issued), expires_at($expires), request_time($now),
  $issued <= $now, $now <= $expires,
  token_id($token_id), allowed_path_prefix($prefix), request_path($path),
  $path.starts_with($prefix);
deny if true;
"#;

const EGRESS_POLICY: &str = r#"
allow if tenant_owner($tenant), request_tenant_owner($tenant),
  workload($workload), request_workload($workload),
  subject_peer($subject), request_subject_peer($subject),
  audience_peer($audience), request_audience_peer($audience),
  role($role), request_role($role),
  operation("egress"), request_operation("egress"),
  issued_at($issued), expires_at($expires), request_time($now),
  $issued <= $now, $now <= $expires,
  token_id($token_id),
  allowed_egress_host($host), request_egress_host($host),
  allowed_egress_port($port), request_egress_port($port);
deny if true;
"#;

impl WorkloadCapabilityClaims {
    fn validate(&self) -> Result<()> {
        validate_value(&self.tenant_owner, "tenant owner")?;
        validate_value(&self.workload_id, "workload ID")?;
        validate_value(&self.subject_endpoint, "subject endpoint")?;
        validate_value(&self.audience_endpoint, "audience endpoint")?;
        validate_value(&self.role, "role")?;
        validate_value(&self.token_id, "token ID")?;
        ensure!(
            self.expires_at_secs >= self.issued_at_secs,
            "Biscuit expiry precedes issue time"
        );
        ensure!(
            self.ingress_path_prefixes.len() <= MAX_INGRESS_PREFIXES,
            "too many ingress prefixes"
        );
        for prefix in &self.ingress_path_prefixes {
            validate_value(prefix, "ingress prefix")?;
        }
        if let Some(host) = &self.egress_host {
            validate_value(host, "egress host")?;
        }
        match self.operation {
            WorkloadOperation::Ingress => ensure!(
                !self.ingress_path_prefixes.is_empty(),
                "ingress token needs a path prefix"
            ),
            WorkloadOperation::Egress => ensure!(
                self.egress_host.is_some() && self.egress_port.is_some(),
                "egress token needs destination caveats"
            ),
            _ => {}
        }
        Ok(())
    }
}

impl WorkloadAuthorizationContext {
    fn validate(&self) -> Result<()> {
        validate_value(&self.tenant_owner, "tenant owner")?;
        validate_value(&self.workload_id, "workload ID")?;
        validate_value(&self.subject_endpoint, "subject endpoint")?;
        validate_value(&self.audience_endpoint, "audience endpoint")?;
        validate_value(&self.role, "role")?;
        Ok(())
    }
}

fn validate_value(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_AUTHZ_VALUE_LEN,
        "{field} length is invalid"
    );
    Ok(())
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("Biscuit timestamp exceeds i64")
}

fn int_term(value: i64) -> Term {
    value.into()
}
