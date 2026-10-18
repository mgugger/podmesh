# Proposal

## Why

A shared Podman workload network unnecessarily puts unrelated tenants on the same bridge. Preserve `podman kube play` while assigning networks by authenticated owner identity.

## What Changes

- **BREAKING**: Podman deployments create/reuse a managed network per tenant per Podman instance; `--workload-network-prefix` replaces the single-network setting.
- Create networks only on deployment, verify ownership/configuration on reuse, and retain unused networks for explicit operator cleanup.
- Test fixtures connect required proxy/control sandboxes to test tenant networks and clean up their own resources. Production adds no infrastructure attachments.
- Update operator documentation and tests without claiming complete firewall or kernel isolation.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `agent`: default tenant-scoped runtime network creation/reuse.
- `local-deployment`: test-only connectivity and workload metrics on tenant networks; updated deployment instructions.
- `workload-traffic-plane`: follow-up investigation fixes local-catalog rollback by sibling seeds and connection-wide reset amplification from per-stream setup failures.

## Impact

Podman runtime/configuration, real-Podman test fixtures, KVM setup, specs and guides. No wire protocol, scheduler, Kubernetes translation, or crypto changes. Main uncertainty: live pod-sandbox network attachment and DNS under rootless Podman; verify with real execution. Automatic unused-network reclamation and egress firewall policy are out of scope.
