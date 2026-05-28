# Biscuit Capability Tokens for Mesh Authorization (Proxy/Sidecar + Custodian Delegation)

## Problem

The current sidecar↔proxy trust model has improved identity and tenant binding through
`NodeCert` + handshake verification, but authorization is still coarse.

Observed gaps:
- Ingress forwarding has no per-request capability token.
- Egress relay accepts destinations without a tokenized policy decision.
- Sidecar registration is verified for tenant ownership/signature but does not carry
  explicit delegated capabilities (scope, expiry, attenuation).
- Custodian key-release is token-checked today, but there is no explicit attenuated
  delegation model for custodian-to-custodian handoff under overload/failure.
- There is no unified way to delegate narrowly scoped permissions across mesh roles.

This leaves a gap between **who** a peer is (identity) and **what** it is allowed to do
(action-level authorization).

## Why Biscuit

[`biscuit-auth`](https://crates.io/crates/biscuit-auth) provides decentralized, offline-verifiable
authorization tokens with attenuation.

This maps well to podmesh:
- Offline verification by proxy/sidecar (no central auth server required).
- Delegation from tenant owner to workload-specific capabilities.
- Scope reduction across hops (owner → proxy, owner → sidecar, or scheduler-mediated issuance).
- Short-lived tokens for replay risk reduction.

## Solution

Introduce Biscuit capability tokens as an authorization layer on top of existing
NodeCert-based identity checks.

### Core model

1. Keep existing identity checks:
   - sidecar verifies proxy `NodeCert`
   - proxy verifies sidecar registration signature + tenant match
2. Add Biscuit token checks for authorization-sensitive operations:
   - sidecar registration acceptance
   - ingress request forwarding to sidecar
   - egress tunnel establishment at proxy
   - custodian share release and optional custodian-to-custodian delegation
3. Tokens encode scoped canonical facts/caveats (schema v1), including:
   - `tenant_owner(owner_pubkey_b64)`
   - `manifest(manifest_id)`
   - `subject_peer(peer_id)`
   - `operation(register|ingress|egress|release_share|delegate_share)`
   - `issued_at`, `expires_at`, `token_id`
   - operation caveats:
     - ingress: allowed path prefixes
     - egress: allowed destination host/port (or constrained patterns/ranges)
     - custodian delegation: allowed delegate peer, share index, worker binding,
       and max delegation depth

### Proposed token-bearing operations

#### 1) Sidecar registration
`SidecarRegistration` includes `authz_token_b64`.

Proxy verifies token authority + constraints before storing route mappings.

#### 2) Ingress forwarding
`ProxyHttpRequest` includes `authz_token_b64` (or equivalent dedicated auth field).

Sidecar verifies token before forwarding request to localhost app.

#### 3) Egress tunneling
`EgressTunnelRequest` includes `authz_token_b64`.

Proxy verifies token and destination constraints before opening outbound TCP.

#### 4) Custodian share release and delegation
`ShareRequest` (and delegated handoff message when present) includes `authz_token_b64`.

Custodian verifies token constraints before releasing a share. Delegation from custodian A to B
is allowed only through attenuation (delegate target binding + tighter expiry + depth limit).

## Compatibility and rollout

- Biscuit authz is required behavior for covered operations (registration, ingress, egress, custodian share release/delegation).
- Missing/invalid token is denied.
- Existing NodeCert checks remain mandatory and unchanged.
- Tokens are additive authorization controls, not identity replacements.

## Security properties after change

- Authorization becomes explicit and least-privilege.
- Proxy access can be delegated with narrow scope and short TTL.
- Replay risk reduced via nonce/session/expiry caveats.
- Cross-manifest misuse reduced by manifest-bound caveats.
- Egress destination controls become cryptographically portable with the request.
- Custodian delegation becomes explicit, attenuated, and auditable.

## Token schema v1

The normative schema and verifier context are defined in:
- `specs/biscuit-token-schema/spec.md`

Summary:
- Token wire field: `authz_token_b64` (base64url Biscuit bytes)
- Required authority facts:
  - `tenant_owner`, `manifest`, `subject_peer`, `operation`, `issued_at`, `expires_at`, `token_id`
- Required verifier ambient facts:
  - `ctx_transport_peer`, `ctx_manifest`, `ctx_path` (ingress), `ctx_dest` (egress),
    `ctx_worker_peer` / `ctx_share_index` (custodian), `ctx_now`
- Deterministic decision checks:
  - signature chain, tenant binding, manifest binding, peer binding, operation binding,
    time window, operation caveats

## Open questions

- Issuer key model:
  - owner key directly signs Biscuit root tokens, or
  - dedicated tenant auth key referenced by owner cert.
- Distribution model:
  - `podctl` mints and injects tokens in workload metadata, or
  - scheduler issues derived/attenuated tokens at dispatch.
- Revocation model:
  - short TTL only, or
  - epoch/version checks distributed through DHT/store.

## Non-goals

- Replacing NodeCert/handshake identity model.
- Global online introspection service.
- Full RBAC redesign for all podmesh APIs in this change.
