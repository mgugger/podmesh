# Biscuit-Based Authorization for Mesh Traffic (Sidecar/Proxy + Custodian Delegation)

## ADDED Requirements

### Requirement: Sidecar registration carries a capability token

`SidecarRegistration` MUST include a Biscuit token authorizing registration for the
specific tenant/workload context.

#### Scenario: Registration token present and valid
- Given a sidecar sends `SidecarRegistration` to a proxy
- And the message includes `authz_token_b64`
- When the proxy verifies token signature and caveats
- Then it accepts registration only if token permits `operation = register`
- And token `manifest_id` equals `SidecarRegistration.manifest_id`
- And token `sidecar_peer_id` equals transport peer identity

#### Scenario: Missing token
- Given a sidecar sends `SidecarRegistration` without `authz_token_b64`
- When proxy processes the registration
- Then proxy rejects the registration

### Requirement: Ingress forwarding is token-authorized at the sidecar

Every ingress request sent from proxy to sidecar MUST carry a Biscuit token scoped
for ingress forwarding.

#### Scenario: Ingress request authorized
- Given sidecar receives `ProxyHttpRequest` containing `authz_token_b64`
- When sidecar verifies the token
- Then it forwards to local app only if token permits `operation = ingress`
- And token `manifest_id` matches the receiving workload
- And requested path/route satisfies token caveats

#### Scenario: Ingress request denied by caveat
- Given token permits only `/api/*`
- When proxy sends request for `/admin`
- Then sidecar rejects the request

### Requirement: Egress tunneling is token-authorized at the proxy

Every egress tunnel request from sidecar to proxy MUST carry a Biscuit token scoped
for egress access.

#### Scenario: Egress request allowed
- Given sidecar opens `/podmesh/egress-tunnel/1.0.0` with token
- And token permits `operation = egress`
- And token allows destination `api.example.com:443`
- When proxy verifies token and destination caveats
- Then proxy establishes outbound TCP tunnel

#### Scenario: Egress destination not allowed
- Given token does not allow destination `169.254.169.254:80`
- When sidecar requests that destination
- Then proxy rejects the tunnel request

### Requirement: Custodian share release and delegation are token-authorized

Custodian share release MUST require a valid Biscuit token, and custodian-to-custodian
handoff MUST be expressed as attenuated delegation.

#### Scenario: Custodian releases share with valid token
- Given a custodian receives a share-release request with `authz_token_b64`
- And token permits `operation = release_share`
- And token `manifest_id` matches the requested manifest
- And token `worker_peer_id` matches the requesting worker transport identity
- When custodian verifies token signature and caveats
- Then it releases the share according to existing key-release flow

#### Scenario: Custodian delegation accepted only for bound delegate
- Given custodian A delegates to custodian B using a token with `operation = delegate_share`
- And token includes `delegate_peer_id = B`, `share_index = i`, and bounded expiry
- When custodian B verifies the delegation token
- Then B accepts delegated handoff only if transport identity is B
- And only for the bound `manifest_id` and `share_index`

#### Scenario: Custodian delegation rejected when depth exhausted
- Given a delegation token has `max_delegation_depth = 0`
- When another delegation attempt is made
- Then the delegation is rejected

### Requirement: Token validation is bound to tenant identity model

Biscuit authorization MUST be evaluated only after existing NodeCert-based tenant
identity checks pass.

#### Scenario: Identity valid, token invalid
- Given sidecar/proxy identities pass NodeCert checks
- And token verification fails
- When request is processed
- Then request is denied

#### Scenario: Identity invalid, token valid
- Given token is structurally valid
- But NodeCert tenant binding fails
- When request is processed
- Then request is denied

## CHANGED Requirements

### Requirement: Verified peer identity is not sufficient authorization

Previously, sidecar/proxy/custodian flows accepted traffic based mainly on peer identity and
registration state. This is no longer sufficient for ingress/egress/share authorization.

#### Scenario: Identity-only request is rejected
- Given a request comes from a peer with valid NodeCert identity
- But no valid Biscuit token is provided
- When system processes the request
- Then request is rejected
