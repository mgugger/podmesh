# Design: Initial local workload DNS

## Status and approval boundary

The user approved implementing the in-workload connector architecture after discussing container and future VM/libkrun execution. The first increment delivered runtime-neutral control/identity inspection with a Podman adapter. Continued implementation adds scoped endpoint enrollment and automatic child renewal. Following the user's offline-owner requirement and continuation after the standing-authority recommendation, deployment uses standing replica/revision-scoped authority with a one-hour maximum child lifetime. Finite parents are supported and cannot be extended by renewal. DNS/discovery and deployment wiring are now implemented and covered by container-free checks; single-host rootless Podman DNS/kernel acceptance has passed; rootful/multi-host and managed-volume acceptance remain pending. Do not ship bearer-authenticated discovery as an interim shortcut.

### Implemented runtime boundary

- `shared/protocol/src/workload_connector.rs` defines bounded/versioned inspection frames and transport-key-signed identity evidence bound to a fresh owner-command challenge.
- `WorkloadRuntime::connector_exchange` transports the same typed request to a connector inside the workload. Unsupported backends explicitly return Unsupported; no generic exec API is exposed.
- The Podman adapter inspects only the owned pod, selects its exact injected connector, and runs the fixed binary subcommand without a shell. Pipes, deadlines and responses are bounded; diagnostics do not reflect payloads.
- The live sidecar serves a private Unix socket inside its filesystem. Startup failures are fatal; shutdown/cancellation cleans the socket and handlers.
- Owner-authorized `ConnectorInspect` and `podctl get connectors <deployment>` collect actual endpoint IDs through existing encrypted control. Agent and CLI verify signatures, workload/owner and challenge bindings. Pending reports remain key-possession evidence only. Enrolled reports include public endpoint-bound child chains, which require live chain/scope verification before use as discovery authority.
- A guest/runtime can configure `PODMESH_CONNECTOR_IDENTITY_DIR` for locally generated persistent keys. Podman injection selects persistent keys in the sidecar writable layer: normal restart preserves keys, but deleting/recreating that layer loses them. No managed volume is provisioned. The agent persists the key binding and refuses silent rebinding; a new owner generation permits an explicit transition.
- The rootless three-replica kube-play test has executed successfully with connector inspection before and after sequential updates. That historical connector check did not exercise DNS or direct peer routing. No VM backend or attestation is enabled. Delegated child renewal is implemented in the continued enrollment increment.

Future VM backends implement the runtime transport using a constrained guest-local channel (for example vsock). Identity keys, DNS/discovery and encrypted session termination remain inside the guest; host attachment is not an identity issuer or confidential traffic terminator. The local control transport is not itself proof of guest identity.

Accepted decisions D-03, D-05, D-08, D-09, D-10, D-14 and D-22 remain applicable. This proposal deliberately supersedes bearer workload credentials in D-07, explicit workload renewal in D-12, and the prohibition on workload-plane record dissemination. It retains trusted execution hosts and shared-network limitations in D-01/D-11; it does not claim confidential execution or self-healing.

## Approved container-free DNS increment

The user requested implementing the local DNS/three-endpoint checks and explicitly deferring Podman
checks. The initial increment exposed library APIs; the subsequent approved continuation wires normal deployment DNS while still deferring Podman tests. No network namespace,
host resolver configuration, nftables or production infrastructure is modified by these tests.

- `shared/protocol/src/service_discovery.rs`: owner-signed public Service catalogs plus signed replica
  leases bound to credential owner/workload/revision, endpoint and application signing key. A member
  cannot invent service names/target ports using tenant membership alone.
- `service_cache.rs`: bounded catalog/record admission, sequence/equivocation checks, 52-character
  full-digest route labels, stable per-instance VIPs, lease/credential expiry and setup-only rotation.
  Owner control explicitly replaces catalogs/generations. Atomic persisted VIP mappings preserve addresses across restart and replacements; non-default IPv4 route conflicts are refused before interception.
- `dns.rs`: Hickory request/response parsing and bounded loopback UDP/TCP transport. Hickory resolver
  captures original system upstream configuration. Forwarding/cache is bounded and non-recursive:
  CNAMEs are returned rather than chased so an external alias cannot leak internal names upstream.
  Truncated UDP retries TCP; query/source/ID are checked and DNSSEC authenticity is not claimed.
- `peer_connector.rs`: peer-only ALPN and operation/direction-specific signed envelopes, mutual bound
  credentials, paginated synchronization with a total deadline, and allowed loopback TCP targets.
  It uses existing raw-relay supervision with finite limits and authority rechecks. Production workload networking refreshes leases and synchronizes up to four neighbors every ten seconds and on authority changes.
- `podmesh-sidecar/tests/local_dns.rs` / `local_peer_dns.rs`: deterministic fake upstream and three
  actual Iroh endpoints/applications on loopback; sparse introduction, DNS/VIP dispatch, remote
  replica proof, port translation, failure retry/expiry and foreign-tenant wire refusal.

Peer envelopes have their own payload types/wire names. They reuse the bounded control frame
encoding on a separate ALPN; they are not proxy handshake requests. Bare self-signed records or
legacy bearer credentials cannot authorize discovery/peer sessions.

The approved deployment continuation now attaches these APIs to the normal sidecar: owner-signed
catalogs travel in encrypted execution metadata; owner control delivers signed peer seeds; original
upstreams are captured before workload-local UDP/TCP DNS interception; reserved VIP TCP dispatch
precedes external proxy selection. Bypass-mark sockets prevent DNS recursion. IPv4 and available
IPv6 DNS rules precede UID/network exemptions, including same-UID apps. All listeners bind before
rule installation and failed installation unwinds it. State uses the connector writable layer;
managed-volume persistence remains deferred. Bootstrap has a total deadline and retains placements
for retry. The production-path in-process test proves local control, DNS, persistence and real remote
Iroh routing without a live proxy, with kernel rules disabled. Kernel-sensitive tasks 1.5/5.4/7 and
real-container acceptance are not implied tested by these checks. The subsequent user-approved
Podman verification executes a Rust probe in real workload network namespaces: same-UID UDP/TCP
DNS, runtime upstream bypass, remote VIP TCP/port translation, all-proxy outage, replica expiry,
ordinary sequential updates and teardown pass on one rootless host. Filters must precede DNAT
expressions; a regression test now checks expression order. Bounded peer connection reuse avoids
repeated transport setup during relay outage, while each stream verifies current bound credentials
and envelopes. Anti-entropy ignores authenticated older sequences without regressing cache state.
Sequential update control waits for the replacement's enrolled record at the requested revision
before replacing the next replica. Proxy grants remain in-memory and require restoration after
proxy process restart. See docs/local-dns.md and verification.md for limitations/results.

## VM regression verification scope - 2026-10-18

The user requested running all existing VM/libkrun tests against the DNS implementation, then
building a workload-to-workload DNS sample once the baseline works. Rebuild current guest
binaries and preserve the existing two/eight-VM direct/forced-relay assertions. Small fixture
fixes needed for the current connector contract are in scope; substantial runtime changes or
the sample design require a separate approval checkpoint. VM-hosted Podman is not a native VM
workload backend, hardware attestation or a physical multi-host claim.

After reviewing the intermittent forced-relay failures, the user explicitly approved diagnosing
and fixing confirmed scheduler/lifecycle causes, adding focused regressions and repeating both
VM modes before implementing a DNS E2E sample. The approved sample uses separate same-owner
client/backend workload pods, real application DNS under `svc.podmesh.internal`, distinct
Service/targetPort values, direct and forced-relay checks and non-forced cleanup. Preserve
authorization and private-name fail-closed behavior; no public DNS fallback or VM-backend claim.

The sample exposed two coupled runtime defects. Background enrollment now holds a distinct
mutation kind: owner lifecycle calls wait only behind enrollment, bounded by the runtime operation
timeout, then reverify request/grant freshness and reread active state. Competing owner mutations
remain fail-fast; cancellation drops guards without releasing workload accounting. Enrollment
still inspects restarted sidecars and cannot issue concurrently with replacement/deletion.

Sidecars refresh signed discovery records on endpoint address changes as well as the existing
interval/authority events. When proxy records configure relay transport, owner bootstrap withholds
the service record until its signed endpoint includes a relay hint; identity evidence remains
available. This is bootstrap readiness, not a live-proxy prerequisite for DNS or peer traffic.
Direct-only configurations still publish direct records. A production-path test exercises seeded
DNS/TCP with a configured but unavailable relay. No wire fields or authorization policy change.

The VM restart check waits up to 45 seconds for all three replica statuses, logging pending errors.
HTTP health only proves that the API is listening, not that asynchronous peer rediscovery finished.
Normal-network runs permit direct UDP but do not assert that workload transport selected it;
forced-relay runs deny direct UDP and require request failure/recovery when relays are blocked.

### Approved PostgreSQL/frontend VM follow-up

The user explicitly approved adding and running real two-tier VM scenarios after learning that
the existing sample tests cover mock workloads and host Podman only. Reuse the eight-VM harness
and actual `deploy/demo_two_tier_{db,frontend}.yml` manifests, not substitute HTTP fixtures.
Add separately selectable normal-network and forced-relay tests requiring a preloaded PostgreSQL
image archive. The first real runs showed that database plus injected sidecar requests 600m CPU
and 384Mi memory, exceeding the lean profile's 50% admission ceiling. The two-tier profile gives
only agent VMs 2 vCPUs/1024Mi RAM with matching 2000m/1024Mi agent capacity; reservation policy
remains unchanged. It runs the actual sample instead of the lean profile's HTTP/lifecycle phases.
Those existing scenarios remain separately selectable and unchanged in scope.

Assert distinct agent VMs via both owner placement and actual guest pods, synthetic resolution
of the frontend's configured database alias, a real `psql` result through frontend ingress, and
failure/recovery of a fresh SQL query when relay transport is blocked/restored. Use non-forced
deletion and verify owned pods are gone. Demo passwords remain fixture-only; diagnostics must not
print connection credentials. Guest Podman/image state is disposable and separate from host
Podman state. Increase guest resources only if measured runtime failures require it. No persistent
database storage, automatic recovery, new production authority or physical multi-host claim.

Fresh guest binaries are required: stale binaries initially rejected the already supported
`emptyDir` manifests. Rebuilding from current source fixed that mismatch without relaxing policy.
The shared HTTP fixture's local identity check now waits for readiness using the inspected backend
pod hostname, retaining exact-response and no-local-fallback assertions.

## Context: observed implementation

| Location | Observed behavior / required change |
|---|---|
| `podctl/src/lib.rs::ReplicaRequest::prepare_execution` | Mints a bearer owner+manifest Biscuit per replica. Default TTL is 30 days; apply considers renewal within one day. No background issuance. |
| `shared/protocol/src/manifest_yaml.rs::normalize_replicas` | Converts replicas=3 into three client-selected placements, each with local replicas=1. Preserve this flow and partial catalogs. |
| `podmesh-agent/src/sidecar.rs::inject` | Sidecar-only base64 environment metadata. The delivered increment adds a private connector socket and read-only runtime inspection; credential/config delivery remains pending. |
| `podmesh-agent/src/service.rs::runtime_manifest` | Revalidates the injected manifest. A managed volume cannot simply be added without a separate trusted-injection validation path. |
| `podmesh-agent/src/runtime.rs::PodmanRuntime::deploy` | Uses `podman kube play --network ... --replace`. Local agents are containers using a remote Podman socket: agent filesystem paths are not automatically host runtime paths. |
| `podmesh-sidecar/src/main.rs` / `identity.rs` | Injected sidecars select ephemeral identity; the library supports a persistent identity directory but injection does not provision one. |
| `shared/iroh_support/src/node_identity.rs` | Iroh transport, application signing, and KEM keys are distinct. Bind keys used for detached records to the endpoint authorization. |
| `podmesh-sidecar/src/iroh_runtime/mod.rs` | Outbound proxy sessions only; no incoming peer accept loop. Every TunnelRequest currently selects a proxy; peer traffic needs a separate dispatch path. |
| `podmesh-sidecar/src/egress_nft.rs` | IPv4 TCP redirection, exclusions for loopback/private runtime network, and blanket sidecar-UID bypass. UDP DNS passes normally. Same-UID applications can bypass current interception. |
| `podmesh-sidecar/src/manifest_routes.rs` | Service and target-port parsing exists for ingress extraction. Extend/reuse it for a service catalog, including Services without Ingress; do not duplicate Kubernetes parsing. |

### Existing three-replica test

`tests/tests/complete_rootless_stack.rs::complete_rootless_stack_serves_ingress` starts the 3-scheduler/3-agent/3-proxy stack with `podman kube play`. It deploys `demo_deployment_without_sidecar.yml`, replaces replicas=1 with replicas=3, asserts three distinct agent EndpointIds in the catalog, and verifies ingress during sequential updates. The delivered increment adds verified self-signed endpoint inspection; the test still does not inspect DNS, enrolled sidecar membership, or inter-workload traffic. The three agents share one Podman host/network, so this is not evidence of separate-host behavior.

The manifest includes `my-nginx-svc` port 80 -> targetPort 80 and `demo-nginx.mesh.local` ingress. Keep the ingress convention unchanged; introduce a distinct non-mDNS DNS suffix. The sidecar image is scratch, so do not assume it has a shell, curl, dig, or a filesystem copy utility.

## Goals / Non-Goals

Deliver application-compatible DNS and authenticated internal TCP routing for the existing fixed-placement workloads, without steady-state proxy or scheduler discovery authority. No owner manipulation of individual keys/tokens; apply performs delegation/bootstrap automatically. Cache state is bounded and reconstructible, not literally absent.

Exclude autonomous placement/replacement, leader election/fencing, hardware attestation implementation, general Kubernetes controllers/selectors, arbitrary application UDP, synthetic IPv6 transport, DHT/Raft, and immediate revocation. Keep identity/enrollment, discovery, and future lifecycle authority distinct. An optional future attestation verifier can strengthen enrollment without changing DNS.

## Decisions

### 1. Establish secure identity before disseminating service records

Prefer sidecar-generated persistent keys. `podctl` issues an owner-rooted delegation bound to the selected agent issuer key and exact owner, workload, replica, incarnation/revision policy, authorized Service catalog digest, allowed operations, and maximum child lifetime. A delegated issuer returns a short-lived endpoint identity credential naming the transport and application signing public keys. Verify parent and child independently, including scope intersection and issuer identity; cap initial delegation depth to one. Use existing Biscuit crates for both tokens with an explicitly verified chain, rather than interpreting holder-added attenuation facts as sub-issuance.

Change proxy workload verification too: new endpoint credentials must match `connection.remote_id()`. Reject old bearer session credentials in the coordinated upgraded environment. Existing endpoint-bound proxy grants retain their separate role and do not become issuers.

Proposed autonomous child renewal is local to the agent, with proof of the current authorized identity. Renewal never extends parent authority, changes service scope, or implicitly permits a new replica incarnation. Endpoint replacement is a separate enrollment transition. Ordinary restarts reuse identity; `kube play --replace` updates need an explicit generation and enrollment policy, not timestamp LWW.

**Chosen issuance policy:** deployment defaults to standing replica/revision-scoped enrollment authority for the already owner-approved agent, with a one-hour maximum child lifetime. The explicit two-Biscuit chain queries authority-block facts only; attenuation cannot manufacture issuer scope. An encrypted enrollment table commits endpoint/signing-key binding before delivery and is removed atomically on deletion. The issuer worker is supervised alongside agent services and retries unavailable workloads. Finite delegation permits autonomous renewal only until its expiry. Indefinitely offline owners require standing delegation or an automated owner authority. Recommend explicitly replica-scoped standing enrollment permission for the workload's lifetime, with short-lived children and agent-side deletion disabling issuance. This does not provide immediate distributed revocation: existing children remain usable until expiry, and a malicious delegated issuer remains a risk. This tradeoff remains explicitly limited to trusted execution agents; repeated issuance never renews an expired finite owner token. Proxy grant expiry/manual recovery remains a separate existing limitation, not silently solved by DNS.

### 2. Runtime-local enrollment, not a machine-plane sidecar

Add a small runtime-mediated exchange for enrollment request/response, current endpoint record and credential/config refresh. No sidecar machine-plane grants or direct podctl connection; no Podman socket in workload containers and no public agent enrollment HTTP port.

The read-only control mechanism has been validated against remote Podman through containerized agents. Keep this a `WorkloadRuntime` transport contract, not a Podman-specific DNS/identity API. Enrollment uses the proven control channel and sidecar writable-layer identity state. A sidecar-only managed named volume remains a separate storage task; no tenant volume-policy exception is implemented. The subcommand communicates with the running sidecar's local Unix socket; it does not rely on a shell. Agent-to-sidecar exec can deliver issuer responses and read signed requests/endpoint records. Polling is bounded and stops when no enrollment/config refresh is pending, except scheduled renewal.

An internal managed volume is not tenant permission to mount volumes. Validate the original manifest under the existing strict policy, then validate exact trusted additions separately; derive volume/container names from full workload identity, account resources, preserve per-workload mutation serialization, and implement cleanup/reconciliation. Confirm remote exec stdin/stdout, permissions, volume persistence across replacement, ownership, timeout/uncertain outcomes, and idempotence before committing the mechanism. If remote exec is unsuitable, request approval for a narrow mailbox alternative; do not guess that hostPath paths in an agent container work.

### 3. Deployment provides initial peers

After each replica starts/enrolls, collect its authorized EndpointRecord via the agent control path. Extend bounded status or add a narrowly scoped typed bootstrap operation; the current receipt has no sidecar endpoint. The delivered ConnectorInspect operation obtains self-signed evidence; seed installation and owner-rooted enrollment are still pending.

Maintain the existing sequential placement/catalog semantics. First replicas may be bootstrap-pending without blocking placement of later replicas. At the end, distribute a bounded connected seed graph; readiness and failure outcomes remain per replica and apply retries converge after interruption. The catalog tracks bootstrap completion, not a live discovery database.

When adding another workload for the same owner, obtain fresh seed records from existing catalog/reconciled workloads so deployments do not form isolated islands. The first three replicas can all receive the other two seeds for the acceptance test; production neighbor count is capped and host diversity is preferred. Addresses include actual sidecar direct/relay hints, not just agent IDs.

Peers exchange authorized candidates thereafter. Cached hints aid restarts but live authorization is always reverified. If every seed is gone, report bootstrap unavailable; cold start cannot discover an unknown private mesh without contact information. Private relay transport remains a separate availability dependency.

### 4. Bounded record synchronization, no consensus

Reuse the sidecar Iroh endpoint with explicit incoming peer acceptance and a dedicated peer ALPN/role boundary. Do not admit a peer as a proxy or accept proxy-only operations from it. Use typed signed envelopes, deadlines, replay registry and stream limits from `shared/protocol`.

Implement Hello, Digest, Fetch and Records over a few authenticated neighbor sessions. Prefer this over adding general gossip membership to the MVP; keep dissemination replaceable by iroh-gossip later. No per-query tenant-wide broadcast or unbounded fanout.

A signed ReplicaServiceRecord includes owner, route/workload ID, service identity and authorized port mappings, replica incarnation, endpoint record, monotonic sequence, issue/expiry and credential chain. Permit signatures only by the credential-authorized signing key. Verify service names/ports against owner-authorized service definitions or their signed digest; possession of tenant credentials is not permission to claim arbitrary services.

Records merge per authorized replica/incarnation. Reject same-sequence equivocation, regressions and excessive future lifetimes; persist sequence or start a newly authorized incarnation after state loss. Expiry is capped by credential validity; renewing credentials refreshes the record too. Bound records/services/peers/pending requests and paginate digests. An absence or suspicion message cannot revoke another replica. Periodic anti-entropy heals reconnects; caller-local reachability backoff provides fast retry before leases expire.

### 5. DNS adapter and names

Use compatible direct dependencies on `hickory-server`, `hickory-proto` and `hickory-resolver` after checking their actual APIs/version/MSRV. Existing lockfile entries are not a direct-dependency declaration. Use a bounded custom request handler; do not implement DNS parsing by hand. Provide bounded forwarded response caching via the resolver and a local synthesized answer path; do not advertise DNSSEC authenticity for unsigned synthesized internal records.

Initial canonical names:

- `<route-label>.svc.podmesh.internal.` for workload service identity.
- `<service-name>.<route-label>.svc.podmesh.internal.` for a supported Service, e.g. `my-nginx-svc.<route-label>.svc.podmesh.internal.`

The existing route ID is a 64-character hexadecimal digest, which exceeds DNS's 63-octet single-label limit. Define route-label as lowercase, unpadded base32 of the same 32 digest bytes (52 characters), preserving the full identity without truncation. Keep the existing hexadecimal routing key and ingress conventions unchanged. Validate each Service name as a supported DNS label rather than silently truncating it.

Route IDs already derive from owner+workload; today owner is the namespace boundary. No independently enforced Kubernetes namespace is invented. Resolve only the injected owner's authorized services. Initially document absolute names; search-domain convenience can be added once Podman behavior is demonstrated. Do not repurpose `.mesh.local`, which is retained for existing ingress only.

Capture the sidecar's original runtime-provided resolver configuration before installing DNS interception. In the local stack this may be Podman's DNS, which forwards public names to the host; do not substitute the containerized agent's resolver or public hardcoded resolvers.

Listen on loopback UDP/TCP 15053. Redirect workload UDP/TCP port 53 requests locally with nftables, preserving the original resolv.conf and avoiding dependence on kube-play DNS field support. Forward only names outside the internal suffix, preserving qtype and bounded DNS semantics; upstream TCP fallback must work. Internal failures never contact upstream DNS. Reject transfers/update requests and bound concurrency, frames, caches, deadlines and EDNS responses. Do not claim DNSSEC validation unless deliberately implemented.

Internal A queries return a stable local service VIP. IPv4 is the initial transport: known internal AAAA returns NODATA rather than a fabricated unreachable address. Known service with no eligible replicas returns SERVFAIL; unknown service also returns SERVFAIL when catalog completeness is unknown. NXDOMAIN is allowed only with owner-authorized catalog evidence, not an empty eventual cache. Unsupported record types for known services return appropriate NODATA/refusal; SRV/headless service behavior is deferred.

### 6. Synthetic service routing

Allocate VIPs from a configured, reserved IPv4 range, checked for conflict with runtime/excluded networks. Persist a bounded mapping from service identity to VIP in sidecar-managed state; do not recycle VIPs across service identities during that identity's lifetime. Old application DNS caches must never route to a different service; a state-loss/replacement transition must preserve mappings or safely fail stale VIPs before assigning new mappings.

Dispatch synthetic destinations before ordinary egress proxy selection and independent of proxy-session availability. VIP+service port maps to eligible authenticated replicas and their targetPort. On the receiver only dial the authorized loopback application target; never accept arbitrary addresses/ports in a peer tunnel request. Support Services without Ingress and named/numeric targetPort resolution using the existing parser. Retry another replica only before successful setup/application-byte forwarding; never replay an arbitrary application request after partial transmission.

Use existing bounded raw relay supervision, cancellation and periodic authority checks. Self-selected replicas take an equally validated local loopback path. Unknown or exhausted reserved-range destinations fail closed, never go to proxy egress. External TCP retains the existing proxy path.

### 7. Fix interception ownership, do not assume UID isolation

Current `meta skuid <sidecar_uid> accept` also exempts applications with that UID; stock nginx can run as root just like the sidecar. DNS and synthetic routing rules must not inherit this bypass.

Preferred bypass is an explicit socket mark on sidecar-originated upstream DNS and outbound TCP that needs bypass, checked before redirect rules. Verify Hickory's custom outbound socket/provider integration and rustables mark support in the first increment. Keep DNS/synthetic interception before unrelated network exclusions; avoid intercepting Iroh UDP except deliberate DNS-port matches. Exercise same-UID application and sidecar scenarios. A packet mark is a loop-avoidance mechanism, not tenant isolation against an application with capabilities to set marks; retain the shared-network threat-model limit. If a reserved non-root UID is chosen instead, it needs explicit policy enforcement and proof that NET_ADMIN works under rootless Podman; changing only a UID in the manifest is not sufficient.

Listener startup/bind failure must fail readiness and unwind rules/tasks. Startup DNS-before-ready returns internal SERVFAIL, never falls back to public resolution. Direct peer listener/address hints remain scoped and bounded; no host-published DNS port is needed.

## Test strategy and acceptance

### Cheap checks first

Protocol/adversarial/property tests: issuer scope intersection, wrong endpoint/owner/signing key, parent expiry, role confusion, public token copied to another endpoint, detached record substitution, generations/sequences, replay, service-port restrictions, deterministic merge and bounded state.

DNS tests use a deterministic fake upstream: internal success, known empty and incomplete cache, unknown names, foreign-owner names, no internal forwarding, external A/AAAA/CNAME/TCP fallback, truncated/oversized packets, timeouts, concurrency and cancellation. VIP tests cover stability/collisions/no reuse, stale destinations, service vs targetPort and no retry after bytes.

In-process three-sidecar integration starts with sparse seeds, converges by anti-entropy, routes raw TCP without proxies, renews short-lived child credentials without podctl, rejects bad peers, reconnects and removes expired replicas. Verify DNS/peer traffic after stopping agent renewal service only within current credential validity; expiry must eventually fail closed.

### Real Podman three-replica acceptance

Extend `complete_rootless_stack.rs` or add `podman_workload_dns.rs` reusing its guards/helpers; keep existing ingress/update coverage. Add a purpose-built fixture with three replicas of the existing Service and a non-root client container with DNS/TCP tooling. Do not depend on tools inside the scratch sidecar or install packages from the internet during tests. A small Rust probe container/binary is acceptable. Also test same-UID interception intentionally.

1. Assert three agent placements and three authorized sidecar identities (not three physical hosts).
2. From workload namespaces, query the Service FQDN over UDP and TCP and obtain reserved VIPs, never host/pod addresses.
3. Query a deterministic non-internal upstream name and Podman network name through the preserved resolver; retain transparent external egress regression. Live public-domain checks are optional, not required CI DNS evidence.
4. Force remote backend selection/replica-specific identity responses so a local nginx response cannot pass as peer routing. Validate port translation and direct Iroh peer counters/observability, not just HTTP 200.
5. After setup completes, stop proxies for a bounded direct-path test on the shared network. Existing proxy-hosted relays stop too: prove direct Iroh connectivity first, and do not claim NAT relay independence. Restore proxies/grants before separate ingress checks.
6. Leave podctl inactive. Stop one sidecar/replica; verify retry to surviving replicas and eventual record expiry without rescheduling. A stopped agent process alone does not imply its workloads died.
7. Restart the sidecar and perform sequential updates; verify identity/generation rules, refreshed credentials, stable service VIP routing and no duplicated old replica membership.
8. Use a separate owner to demonstrate that DNS/discovery/tunnel authority cannot cross tenants. Shared-network direct application access is still not isolated.
9. Verify automatic child renewal with shortened test lifetimes, deletion and managed state cleanup. Test delegation expiry separately from child renewal; do not infer indefinite owner authorization.

## Risks / Trade-offs

- [Enrollment/managed state is larger than DNS parsing] -> Land a security/runtime feasibility increment first; keep DNS cache tests independent while that gate is reviewed.
- [Standing delegated issuer can act maliciously within scope] -> Exact per-replica grants, short children, explicit threat model; confidential enrollment is future work, not a present guarantee.
- [Runtime host path confusion or policy weakening] -> Remote-socket Podman checks and exact internal resource validation; tenant volumes remain prohibited.
- [Clock/sequence rollback or lost keys] -> Bound skew/lease validity and require deliberate incarnation changes; no latest-wallclock-wins replacement.
- [Gossip partitions / stale credentials] -> Redundant seeds, anti-entropy, expiry, local retry; no global failure or instant revocation promise.
- [Shared UID and resolver loops] -> Explicit bypass feasibility tests before user-facing DNS interception.
- [Local Podman hides separate-host reachability] -> Keep multi-host/KVM verification separate and unclaimed until executed.

## Implementation ordering

Use tasks.md: feasibility and authority approval -> credential/enrollment -> deployment seeds -> record synchronization -> DNS adapter -> direct tunnel dispatch -> real Podman acceptance. Each increment includes its tests/docs. No source guarantees change until implementation and acceptance; no legacy bearer fallback. Coordinate wire/store upgrades as permitted by D-18.
