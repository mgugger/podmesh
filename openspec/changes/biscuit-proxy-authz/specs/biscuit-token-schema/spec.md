# Biscuit Token Schema

## ADDED Requirements

### Requirement: Token authority facts bind tenant, workload, peer, operation, and time

Tokens MUST contain these authority facts:

- `tenant_owner(<owner_pubkey_b64>)`
- `workload(<workload_id>)`
- `subject_peer(<node_id>)`
- `operation(<register|ingress|egress>)`
- `issued_at(<unix_secs>)`
- `expires_at(<unix_secs>)`
- `token_id(<opaque_id>)`

Ingress tokens additionally constrain path prefixes. Egress tokens additionally constrain
destination host and port. Verifiers bind these facts to transport identity, NodeCert tenant,
request workload, operation, and current time.

#### Scenario: Required fact is missing
- **WHEN** a verifier receives a token missing tenant, workload, peer, operation, issue time, expiry, or token ID
- **THEN** verification fails closed

### Requirement: Ingress tokens constrain paths

Ingress tokens MUST contain allowed path-prefix caveats evaluated against the requested HTTP path.

#### Scenario: Ingress prefix matches
- **WHEN** the requested path matches an allowed token prefix and all identity facts match
- **THEN** path authorization succeeds

### Requirement: Egress tokens constrain destinations

Egress tokens MUST contain destination host and port caveats evaluated against the requested tunnel.

#### Scenario: Egress destination differs
- **WHEN** either requested destination host or port differs from token scope
- **THEN** destination authorization fails