# Design

## Approved scope

The user explicitly approved implementation after review: every tenant gets a network on actual deployment, as standard behavior; tests arrange required connectivity. Keep Podman/kube play. No production test-network hooks or new Kubernetes layer.

## Runtime

Use a validated configurable prefix plus the full BLAKE3 digest of the decoded owner public key. The owner comes from authenticated deployment state. Create a DNS-enabled, non-internal bridge using Podman's idempotent `network create --ignore`, then inspect it and verify managed/tenant labels, driver and DNS/internal settings before use. This handles agents sharing a socket without an in-memory authoritative map. Network selection does not alter workload runtime IDs or lifecycle operations.

Retain networks after delete: another agent may still use them. Operators remove verified unused networks explicitly. Network objects/address pools are finite and owner churn remains a documented limitation. No migration or mixed-version compatibility is promised.

## Tests

Shared fixtures can prepare the expected tenant network and attach the control/proxy pod infra containers with their original DNS aliases before apply. Restart the disposable proxy pod after live interface attachment, before grants are minted, and wait for its APIs: otherwise Iroh records may retain only the old bridge addresses. This preserves relay names, external DNS fallback and transparent egress targets. Metrics probes join the tenant network. A separate real-Podman test exercises missing-network creation, concurrent creation, reuse, different owners, conflicting networks, and exclusive workload attachment. Guards remove only test-owned networks/attachments, after workloads.

KVM fixtures retain routable guest-IP proxy/relay endpoints and no longer precreate a shared workload network.

## Boundaries

Supersedes the shared-workload-network portions of D-01/D-11, not owner authorization or trusted-host assumptions. Distinct bridges are not a complete cross-tenant firewall: proxy-mediated egress and host routing still matter. Production operators must supply reachable proxy/relay endpoints. Automatic reclamation is deferred as agreed.

## Follow-up peer-reset investigation

The user requested investigation of the failing post-update peer TCP test. Focused regressions
confirmed that sibling seeds could displace the local execution catalog and that one failed sync
closed a shared cached QUIC connection, resetting a previously established service tunnel. Preserve
the configured local catalog at the network control boundary and scope setup failures to their
streams. Keep remote owner-authorized catalog replacement, all cryptographic validation, bounds
and runtime deadlines unchanged. Compare optimized pre-fix runtime images separately from fixed
images; see investigation.md for results rather than attributing every reset to bridge topology.

## Checks

Formatting, focused/runtime/property tests, default workspace tests, Clippy, OpenSpec validation and real Podman execution with current images. Report KVM/rootful checks separately if unavailable. A Podman 6.1.4 rootless feasibility probe confirmed network attachment to a pod infra container with a network-scoped alias; temporary probe resources were removed.
