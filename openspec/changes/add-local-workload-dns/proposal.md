# Proposal: Local workload DNS and direct peer traffic

## Why

Workloads currently register with ingress/egress proxies but cannot resolve tenant services into authenticated sidecar-to-sidecar Iroh connections. Add a local DNS compatibility layer over rebuildable, signed discovery state, using the existing three-replica Podman workflow as the acceptance case.

## What Changes

- Add loopback-only UDP/TCP DNS using Hickory, internal service virtual IPv4 addresses, and forwarding of non-internal queries to the workload's original runtime/host resolver.
- Add authenticated, bounded sidecar peer sessions, signed expiring service records, record synchronization, and direct TCP tunnels. Internal lookup/connection failures never fall through to public DNS or proxy egress.
- Bootstrap with actual sidecar endpoint records collected and delivered through existing owner-agent control during deployment, not a permanent proxy introducer. Include existing tenant peers when deploying another service.
- **BREAKING:** Replace bearer workload-session authority with endpoint-bound identity credentials, including the authorized application signing key. Use an owner-rooted, replica-scoped delegated enrollment chain; automatically renew child credentials locally while parent authority remains valid.
- Add a narrow runtime-mediated enrollment/configuration channel and sidecar-owned persistent identity storage without exposing the Podman socket to workloads or accepting tenant-authored volumes.
- Extend real Podman tests to resolve services, connect directly between replicas, survive a peer loss and sequential update, and retain external DNS/egress and ingress behavior.

## Capabilities

### New Capabilities

- None. DNS/discovery belongs to the existing workload traffic plane.

### Modified Capabilities

- `workload-traffic-plane`: split DNS, decentralized service discovery, endpoint-bound peer authority, and internal TCP routing independent of proxy sessions.
- `message-security`: delegated endpoint credentials, authorized signing-key binding, and bounded discovery/tunnel control envelopes.
- `agent`: runtime-mediated enrollment and managed identity state; maintain rejection of tenant-supplied host access and volumes.
- `podctl-cli`: automatic deployment delegation, seed exchange, resumable bootstrap, and partial-failure reporting without an Iroh endpoint.
- `local-deployment`: three-replica DNS and direct-traffic acceptance checks on the existing Podman topology.

## Impact

Affected: `shared/protocol`, `shared/crypto`, `shared/iroh_support`, `podctl`, `podmesh-agent`, `podmesh-sidecar`, proxy credential verification, bounded metrics, deployment documentation, and tests. Add direct Hickory dependencies with versions aligned to existing Iroh dependencies; reuse Iroh, Biscuit, postcard, and rustables. No Raft, DHT, external DNS service, new public agent API, or scheduler-owned service registry.

The user approved implementing the in-workload connector architecture. Its runtime-neutral inspection boundary and Podman adapter are delivered as the first increment; the full DNS rollout is not complete. Enrollment/delegation lifecycle remains a security-critical approval gate described in design.md. Autonomous rescheduling, hardware attestation, arbitrary application UDP, service sharding, immediate distributed revocation, and tenant network isolation are excluded. Future controllers and confidential execution must fit the authority model without granting lifecycle powers to ordinary discovery peers.
