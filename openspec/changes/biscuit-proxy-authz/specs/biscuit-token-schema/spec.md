# Biscuit Token Schema v1 for Mesh Authorization (Proxy/Sidecar + Custodian Delegation)

## ADDED Requirements

### Requirement: Token envelope format is stable and interoperable

All podmesh authz tokens MUST be transported as base64url-encoded Biscuit bytes in
an `authz_token_b64` field on the relevant wire message.

#### Scenario: Token transport
- Given a token is attached to `SidecarRegistration`, `ProxyHttpRequest`, `EgressTunnelRequest`, or custodian share/delegation messages
- When serialized on the wire
- Then `authz_token_b64` contains URL-safe base64 (no padding required)
- And receivers decode it to raw Biscuit bytes before verification

### Requirement: Authority block contains required podmesh facts

The authority block MUST include these facts:
- `tenant_owner(<owner_pubkey_b64>)`
- `manifest(<manifest_id>)`
- `subject_peer(<peer_id>)`
- `operation(<"register"|"ingress"|"egress"|"release_share"|"delegate_share">)`
- `issued_at(<unix_secs>)`
- `expires_at(<unix_secs>)`
- `token_id(<opaque_id>)`

#### Scenario: Missing required fact
- Given a token omits one required fact
- When verifier evaluates the token
- Then verification fails

### Requirement: Token includes operation-specific caveats

#### Scenario: Register token caveats
- Given `operation("register")`
- Then token MUST include caveat binding `subject_peer` to transport peer_id
- And caveat binding `manifest` to `SidecarRegistration.manifest_id`

#### Scenario: Ingress token caveats
- Given `operation("ingress")`
- Then token MUST include one or more allowed path prefixes
- And verifier MUST deny requests whose path does not match allowed prefixes

#### Scenario: Egress token caveats
- Given `operation("egress")`
- Then token MUST include destination constraints (`dest_host`, `dest_port` or pattern/range)
- And verifier MUST deny requests outside constraints

#### Scenario: Share-release token caveats
- Given `operation("release_share")`
- Then token MUST include `worker_peer_id` and `share_index` bindings
- And verifier MUST deny release when worker identity or share index differs

#### Scenario: Delegation token caveats
- Given `operation("delegate_share")`
- Then token MUST include `delegate_peer_id`, `share_index`, and `max_delegation_depth`
- And verifier MUST deny when delegate peer differs or depth is exhausted

### Requirement: Verifier ambient facts are canonical

Verifiers MUST provide ambient facts from runtime context:
- `ctx_transport_peer(<peer_id>)`
- `ctx_manifest(<manifest_id>)`
- `ctx_path(<http_path>)` (ingress only)
- `ctx_dest(<host>, <port>)` (egress only)
- `ctx_worker_peer(<peer_id>)` (custodian release/delegation)
- `ctx_share_index(<u32>)` (custodian release/delegation)
- `ctx_delegate_peer(<peer_id>)` (delegation only)
- `ctx_now(<unix_secs>)`

#### Scenario: Time validity
- Given `ctx_now > expires_at`
- When verifier evaluates token
- Then request is denied as expired

### Requirement: Base verification policy is deterministic

All components MUST apply equivalent checks:
1. Biscuit signature chain verifies under configured root public key.
2. `ctx_now` is within `[issued_at, expires_at]`.
3. `tenant_owner` equals tenant identity established by NodeCert checks.
4. `manifest` equals request/receiver manifest context.
5. `subject_peer` equals connection/request peer context.
6. `operation` equals requested action.
7. Operation-specific caveats pass.
8. Delegation-specific checks (if operation is `delegate_share`) pass: delegate binding and depth > 0.

#### Scenario: Same token, same context
- Given proxy and sidecar evaluate with equivalent context
- When using the same policy rules
- Then both produce the same allow/deny result

### Requirement: Root key binding is explicit

Token verification MUST use tenant-scoped root keys selected by tenant owner identity.

#### Scenario: Wrong root key
- Given a valid token signed by tenant A root
- And verifier selects tenant B root key
- When verification runs
- Then verification fails

## CHANGED Requirements

### Requirement: Unstructured token contents are not acceptable

Ad-hoc/custom payload claims without required podmesh facts are insufficient.

#### Scenario: Token with custom claims only
- Given a token lacks required canonical facts (`tenant_owner`, `manifest`, `subject_peer`, `operation`, `issued_at`, `expires_at`, `token_id`)
- When verifier processes it
- Then request is denied
