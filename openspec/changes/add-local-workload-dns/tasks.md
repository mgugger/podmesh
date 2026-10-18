# Implementation plan

The connector, endpoint enrollment and DNS/discovery deployment wiring are implemented incrementally; single-host rootless DNS/kernel acceptance has passed; rootful/multi-host checks, managed volumes and broader readiness/metrics remain incomplete. This is the only owning task list for this change. Dependencies are intentional: no peer discovery is enabled with bearer credentials. See design.md for source trace, approval scope and limitations.

## 0. Runtime-neutral in-workload connector increment

- [x] 0.1 Define bounded/versioned connector inspection and fresh transport-key-signed identity evidence in shared/protocol; verify strict parsing, size limits, challenge/owner/workload/signing-key binding and property round trips.
- [x] 0.2 Implement private in-workload Unix control and a shell-free connector binary adapter; verify permission checks, malformed requests, stale sockets, startup failure, shutdown and cancellation cleanup.
- [x] 0.3 Add typed WorkloadRuntime connector transport and the Podman adapter with exact owned-container selection, finite pipes/deadlines and secret-safe failure reporting; verify focused adapter tests.
- [x] 0.4 Expose owner-authorized ConnectorInspect and podctl get connectors, validating evidence in both agent and CLI; verify wrong owner, stale challenge, tampering, unsupported backend and CLI parsing tests.
- [x] 0.5 Verify locally generated persistent identity across restart and operation without a live proxy in the in-process sidecar test; document that managed Podman identity storage and owner-rooted enrollment are not delivered.
- [x] 0.6 Execute the existing rootless three-replica kube-play test with connector inspection before/after sequential update; retain existing ingress/reconciliation/deletion checks and document the single-host limit.
- [x] 0.7 Complete final workspace regression, Clippy and updated-image Podman checks after cancellation cleanup changes; recorded 518 passing normal tests, passing workspace Clippy/format checks and both executed rootless Podman checks in verification.md. Owning-change strict validation passes; unrelated all-project length warnings remain.

## 1. Security and Podman feasibility gate

- [x] 1.1 Continue with the recommended standing replica/revision-scoped enrollment authority following the user's offline-owner requirement and implementation continuation; recorded one-hour children, finite-parent support and revocation/trusted-issuer limitations in design.md.
- [x] 1.2 Implement and verify a bounded two-Biscuit chain: authority-block facts only, explicit issuer/scope intersection, bound transport/signing keys, finite-parent expiry caps and attenuation constraints. Protocol adversarial tests pass.
- [x] 1.3 Exercise sidecar-binary local control subcommands through remote Podman exec with bounded stdin/stdout, timeouts and no shell; verified by the executed complete_rootless_stack test with containerized agents. Credential installation remains task 2.4, not implied by inspection.
- [ ] 1.4 Prove sidecar-only managed named-volume identity persistence across restart/kube-play replacement and deletion; verify original tenant volumes remain rejected and injected fields pass exact internal validation.
- [ ] 1.5 Prove DNS/socket-mark bypass and VIP interception with same-UID application/sidecar processes, including upstream TCP fallback, under rootless/rootful Podman where available; verify no resolver loop and no silent application bypass.
- [ ] 1.6 Record runtime feasibility results, selected channel and authority scope in design.md/verification.md; verify no attestation, host-isolation or infinite finite-token renewal claims are introduced.

## 2. Endpoint-bound identity and enrollment

- [x] 2.1 Add bounded/versioned delegation and identity validators in shared/protocol, including issuer, workload/revision/generation and key binding; focused tests verify malformed/oversized encoding, tampering, scope/role confusion, expiry and attenuation rejection.
- [x] 2.2 Provision scoped enrollment delegation automatically in existing encrypted execution metadata without generating sidecar secrets; property tests verify scope and recipient binding, and agent/sidecar metadata verify selected issuer and revision.
- [x] 2.3 Select persistent keys in the sidecar writable layer and add verified identity installation over local control; authorization tests cover wrong endpoint/signer/generation and renewal regression. Reports carry public bound credentials, never authentication private keys or relay tokens. Managed-volume persistence remains 1.4.
- [x] 2.4 Implement agent enrollment and supervised renewal with per-workload mutation guards and encrypted durable key binding committed before delivery. Focused test verifies owner-offline renewal, agent restart, refusal of silent key changes and atomic binding deletion. Runtime image checks are tracked below.
- [x] 2.5 Replace proxy bearer admission/rechecks with bound chain validation; enforce application signer and replica identity on later operations. Bound renewal tests refuse endpoint/signer/scope changes and regression; in-process ingress/egress tests now use enrolled identities.
- [x] 2.6 Implement automatic child refresh within parent authority and refresh authenticated proxy sessions without changing their keys. Protocol/agent tests verify finite-parent caps and owner-offline short-child renewal; live control/session renewal integration is included in full verification.
- [ ] 2.7 Finish enrollment review, full workspace/Clippy and updated-image Podman checks; synchronize source guarantees only after verified review. Operator notes describe writable-layer keys, standing authority, public credentials and proxy-grant/revocation limits.

## 3. Deployment seed exchange

- [x] 3.1 Add a bounded typed owner-agent bootstrap/status response and seed-update operation in shared/protocol, agent service and runtime; verify target/owner signatures, encrypted transport, size limits and non-owner refusal.
- [x] 3.2 Extend podctl catalog/apply to collect actual enrolled sidecar endpoint records after startup and deliver a bounded connected seed graph; verify three-replica tests do not deadlock placement while earlier replicas await peers.
- [x] 3.3 Join later workloads to authorized existing tenant seeds using catalog/reconciliation and refreshed agent reports; verify two deployments form one discovery mesh and another owner's seeds are rejected.
- [x] 3.4 Persist/retry per-replica bootstrap progress while retaining confirmed ownership and cleanup handles; verify interrupted apply, unavailable agent and partial seed delivery tests, and document bootstrap-unavailable behavior.

## 4. Peer discovery and service cache

- [x] 4.1 Extend the supported Service parser/catalog for standalone Services, service port -> targetPort and owner-authorized advertisement scope; verify numeric/named ports, no Ingress dependency and invalid selectors/aliases fail safely.
- [x] 4.2 Add signed expiring ReplicaServiceRecord encoding and per-incarnation/sequence merge with named bounds; verify expiry, equivocation, sequence regression, invalid authority and bounded cache properties.
- [x] 4.3 Add sidecar incoming peer accept loop, mutual endpoint-bound handshake and role-separated peer ALPN/typed envelope operations; verify foreign owners, proxy-role confusion, replay and unauthorized signing keys are refused.
- [x] 4.4 Implement bounded neighbor management, Digest/Fetch/Records synchronization and authorized candidate exchange; verify three in-process sidecars converge from sparse seeds and reconnect without a proxy or per-query broadcasts.
- [x] 4.5 Implement lease refresh, credential-aware record expiry and caller-local reachability penalties; verify lost peers expire without deleting siblings or turning a suspicion into global revocation, and document partition limitations.

## 5. Local DNS adapter

- [x] 5.1 Add compatible direct Hickory dependencies and a bounded loopback UDP/TCP DNS handler; verify build/MSRV, bind collisions, oversized requests, unsupported DNS operations, deadlines and shutdown tests.
- [x] 5.2 Implement internal FQDN classification and stable bounded VIP allocation with safe restart/no-reuse behavior; verify full-digest base32 route labels (the existing 64-character hex key cannot be a DNS label), label/name length bounds, case/trailing-dot handling, foreign-owner scope, collisions, A/AAAA behavior and stale-address safety properties.
- [x] 5.3 Capture original workload resolver configuration and forward only non-internal names through it; verify deterministic upstream UDP/TCP/CNAME/AAAA, timeout/cache bounds and zero forwarding for failed internal names.
- [x] 5.4 Implement the approved nftables DNS/synthetic routing and outbound bypass integration; verify rules work before UID/network exclusions and unwind on startup failure/cancellation.
- [x] 5.5 Document canonical service names, incomplete-cache SERVFAIL, IPv4-first scope and preserved host/runtime DNS behavior; verify examples against a test fixture rather than claiming full Kubernetes DNS/search semantics.

## 6. Direct internal TCP routing

- [x] 6.1 Dispatch VIP destinations before proxy session selection; verify unknown reserved destinations fail closed and public destinations retain proxy egress.
- [x] 6.2 Add mutually authorized internal tunnel setup and loopback-only service target resolution, using existing raw relay supervision/limits; verify service-port translation, arbitrary-target refusal, cancellation and authority expiry.
- [x] 6.3 Implement replica balancing and setup-only retry; verify remote and self-selected paths, unreachable backend fallback and no retry after any application bytes are forwarded.
- [x] 6.4 Add finite-catalog DNS/discovery/peer-tunnel metrics and readiness state; verify no identity labels/secrets and that internal traffic works with zero proxy sessions, then update docs/p2p-flows.md.

## 7. Real three-replica Podman acceptance

- [x] 7.1 Reuse complete_rootless_stack guards/topology with an explicit DNS/TCP probe fixture, without assuming shell/tools in scratch images or downloading packages; verify exactly three placements and distinct enrolled sidecar identities.
- [x] 7.2 Query internal names via UDP/TCP from workload namespaces and prove remote Iroh TCP routing/targetPort translation; verify replica-specific responses or remote selection plus direct peer metrics, not only HTTP status.
- [ ] 7.3 Verify deterministic external/runtime DNS and existing transparent egress/ingress regressions; verify same-UID DNS/VIP traffic still intercepts and internal failures never reach fake upstream DNS.
- [x] 7.4 With podctl inactive, stop proxies after direct paths are proven and separately stop a workload replica; verify bounded internal availability, setup retry and expiry without rescheduling, restoring proxy authorization before ingress checks.
- [ ] 7.5 Exercise sidecar restart, sequential replica update, short-lived child renewal and a second owner; verify stable service routing, no duplicate old incarnation, endpoint credential isolation and explicit shared-network limitations.
- [ ] 7.6 Verify deletion/reconciliation cleans managed runtime identity state without affecting siblings; record executed rootless/rootful checks separately from compilation or unavailable environments.
- [x] 7.7 Run formatting, targeted tests, workspace tests/Clippy and OpenSpec validation; record exact results and any unexecuted Podman/multi-host checks in verification.md.

## 8. Approved container-free DNS development

Podman/kernel checks are explicitly deferred for this increment. The subsequent deployment wiring
is tracked in section 9; local checks do not establish kernel/runtime acceptance.

- [x] 8.1 Add compatible Hickory dependencies, owner-signed public catalogs, bound signed replica
  records and peer-only typed envelopes; verified production validators refuse tampering, foreign
  owners, scope/signing-key substitution and unapproved ports.
- [x] 8.2 Implement bounded service cache, full-digest DNS labels, stable per-instance VIP mapping,
  sequences/equivocation checks and expiry; verified cache tests and arbitrary-digest property test.
- [x] 8.3 Implement loopback UDP/TCP DNS with pre-parse admission bounds and original-upstream
  capture/forwarding; fake-upstream tests cover internal privacy, external A/AAAA/CNAME, NXDOMAIN,
  timeout, query-ID mismatch, cached replies and UDP truncation/TCP fallback.
- [x] 8.4 Implement mutually authenticated peer sync and direct TCP dispatch using owner catalogs,
  application-key-bound signed envelopes and existing raw-relay supervision; three real loopback
  endpoints prove sparse introduction, replica-specific routing, retry, expiry and wire tenant refusal.
- [x] 8.5 Verify oversized/malformed input, unsupported operations, partial TCP deadlines and shutdown
  cleanup without privileges or containers; document opt-in API/production-integration limitations in
  docs/local-dns.md and docs/p2p-flows.md (subsequently updated for section 9 deployment wiring).
- [x] 8.6 Finish post-review local/workspace tests, Clippy and OpenSpec validation; record executed
  commands/results in verification.md. Do not run Podman tests for this increment.

## 9. Approved deployment wiring without Podman execution

The user requested finishing DNS so it should work with Podman, without running Podman tests.
These implemented paths and local checks do not complete the real-runtime acceptance tasks above.

- [x] 9.1 Compile matching ClusterIP/TCP Services and named/numeric target ports into owner-signed
  catalogs, validate manifest/revision binding, and deliver through encrypted execution/sidecar metadata.
- [x] 9.2 Add owner-authorized bounded SeedPeer control through agent/runtime/connector; collect live
  enrolled records during apply, install a bounded seed graph, introduce existing deployments and retain
  recorded placements on bootstrap failure for retry. Total bootstrap deadline is bounded.
- [x] 9.3 Wire normal sidecar networking: credential-aware lease refresh, bounded neighbor sync,
  stale-hint synchronization only, incoming peer routing, and internal TCP dispatch before proxy selection.
- [x] 9.4 Persist private atomic VIP mappings, discovery sequence and bounded hints beside connector
  identity; restore no eligible backend records; allow explicit catalog/generation replacement without
  address reuse and refuse conflicting non-default IPv4 routes.
- [x] 9.5 Implement workload-local UDP/TCP DNS and IPv4/available-IPv6 interception ahead of UID/network
  exemptions, socket-mark upstream bypass, loopback-only TCP dispatch listeners, pre-bind startup and
  failed-installation cleanup. This checkbox denotes implementation, not exercised kernel behavior.
- [x] 9.6 Add a normal-sidecar in-process test for signed seed control, DNS, persistent mapping and real
  remote Iroh TCP routing without a live proxy or possible local application fallback. Kernel rules disabled.
- [x] 9.7 Finish final reviewed workspace/lint/format/OpenSpec checks and record results (538 normal tests passed); leave real Podman
  tests, managed volumes, DNS readiness/metrics and VM/multi-host acceptance pending.

## 10. Approved real Podman verification and fixes

The user subsequently authorized real Podman execution and fixing observed failures.

- [x] 10.1 Build native images and run real tests serially against a temporary owned rootless API
  service; distinguish one-host rootless results from rootful/multi-host acceptance.
- [x] 10.2 Correct IPv4 DNS/VIP filters to execute before DNAT/register loads; add an expression-order
  regression test and verify real same-UID UDP/TCP DNS and VIP interception.
- [x] 10.3 Ignore authenticated older anti-entropy sequences without accepting equivocation/forgery,
  weakening key/generation checks, regressing state or aborting synchronization batches.
- [x] 10.4 Reuse bounded peer transports, admit concurrent bounded streams, and retain fresh per-stream
  envelope/credential checks. Verify service survives all-proxy outage beyond a stopped replica's lease.
- [x] 10.5 Wait for the replacement's enrolled record at the requested revision before replacing the next
  replica; preserve confirmed placement before waiting. The ordinary uninterrupted-update test passes.
- [x] 10.6 Execute the Rust namespace probe without installing tools in scratch images: UDP/TCP internal
  A, AAAA NODATA, runtime upstream bypass, remote VIP port 18081 -> targetPort 80, setup retry and expiry.
- [x] 10.7 Restore in-memory owner proxy grants after process restart, verify post-outage ingress recovery,
  and retain metrics, single-replica reconciliation, update, deletion and guard cleanup checks.
- [x] 10.8 Run real transparent external-egress regression on the final sidecar image; sidecar/proxy logs
  and HTTP 200 confirm tunneling, not accidental direct application egress.
- [x] 10.9 Finish final normal workspace/feature-inclusive Clippy/format/OpenSpec checks (540 normal tests pass) and record exact
  results and remaining rootful, IPv6-kernel, fake-upstream and managed-volume limitations.

## Workflow follow-up

- Present implementation review and remaining security/runtime limitations before acceptance.
- Update accepted decisions/source specs only for the approved implemented behavior; archive this change after review, without archiving unrelated changes.
