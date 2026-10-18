# Workload-local DNS and direct peer TCP

Agent-injected connectors now receive an owner-signed Service catalog. After endpoint enrollment,
the connector starts local DNS and peer discovery. `podctl apply` collects actual signed connector
records, delivers a bounded seed graph through owner-authorized agent control, and introduces later
deployments to existing tenant peers. The connector subsequently refreshes records and synchronizes
neighbors without `podctl` or a proxy discovery authority.

**Rootless Podman DNS/kernel acceptance has now passed on one physical host.** It verifies
same-UID UDP/TCP DNS, runtime upstream forwarding, remote VIP TCP/targetPort translation, all-proxy
offline operation, stopped-replica retry/expiry, updates and cleanup. Rootful and multi-host/NAT
acceptance remain pending; container-free checks alone still do not prove kernel behavior.

## Container-free checks

```sh
cargo test --locked -p podmesh-sidecar --test local_dns --test local_peer_dns --test production_dns --test peer_stream_isolation
```

Fixtures use production catalog, credential, record and envelope validators, not an authorization
bypass. They start no containers, do not modify host DNS/routes/nftables, and need no Internet DNS.

- `local_dns`: deterministic upstream A/AAAA/CNAME/NXDOMAIN, cache, timeout, mismatched IDs,
  truncated UDP/TCP fallback, internal-query privacy, admission limits and shutdown; signed-record
  security, base32 properties and persisted VIP restoration/catalog replacement.
- `local_peer_dns`: three real loopback Iroh endpoints, sparse introduction, paginated discovery,
  replica-specific remote TCP responses, Service targetPort translation, setup-only retry, expiry
  and foreign-tenant wire refusal.
- `peer_stream_isolation`: a failed sync/catalog validation on a cached transport does not reset
  an already-established service tunnel, and subsequent synchronization remains usable.
- `production_dns`: normal sidecar startup, signed seed installation through connector control,
  refusal of older sibling catalogs for the local deployment, local DNS, durable mapping and
  internal HTTP CONNECT dispatch over real Iroh. Its remote
  application listens on a different loopback alias, so local fallback cannot satisfy the probe.
  No live proxy is available. Kernel interception is explicitly disabled for this test.

## Deployment and runtime boundaries

- Service compilation supports matching ClusterIP/TCP Services, numeric/named target ports and
  selector validation without an Ingress. Unsupported protocols/types are refused. A workload
  without Services gets a canonical `workload` service using declared TCP container ports (or the
  connector's 18080 fallback). This is not general Kubernetes service/controller compatibility.
- Catalogs are owner-signed and bound to the workload manifest/revision. Membership alone cannot
  authorize arbitrary names or target ports. Replica leases require bound Biscuit credentials and
  application-key signatures; bare endpoint records and bearer credentials are insufficient.
- Owner control explicitly selects catalog/generation replacements. Ordinary gossip cannot choose
  a new owner intent. Seeds for the local deployment must match its configured execution catalog;
  an older sibling seed cannot roll back the replacement's own catalog. Agent control/runtime exec
  mediates seed delivery; machine peers do not become discovery members. Bootstrap failure retains recorded placements so `podctl apply` can retry.
  Sequential updates wait for each replacement's live enrolled record at the requested revision
  before replacing the next replica; a deployment response itself is not enrollment readiness.
- `workload_network.rs` refreshes leases every ten seconds and on authority changes, synchronizing
  up to four neighbors. Records expire within 120 seconds and never outlive their credential.
  Stale signed hints can bootstrap synchronization only, never authorize application traffic.
  Up to 32 outbound peer connections and 32 incoming sessions are retained with bounded stream
  admission and idle shutdown. Every stream still carries fresh signed, currently verified authority.
  Setup/synchronization failures are stream-scoped and do not close the shared transport underneath
  other authorized service tunnels. Valid older anti-entropy snapshots are ignored without aborting batches or regressing state.
- VIP mappings, discovery sequence and bounded signed hints are persisted as atomic private files
  beside connector identity. Eligible live backend records are not restored from disk. Writable-layer
  storage survives normal restart, not container recreation; managed volumes remain unimplemented.
- The original resolver configuration is captured before interception. Production Linux nftables
  redirects UDP/TCP port 53 to loopback port 15053 and service VIP TCP to the loopback dispatcher.
  DNS/VIP rules precede UID/network exemptions, including same-UID applications. Marked upstream
  sockets bypass redirects. IPv6 DNS interception is installed when IPv6 loopback DNS is available.
  DNS and TCP listeners bind before rule installation; failed installation unwinds the rules.
- Internal TCP dispatch occurs before external proxy session selection. Unknown reserved VIPs fail
  closed. Peer targets are authorized loopback ports, and retries occur only before application
  bytes are sent. Relays have finite byte/lifetime/idle limits and check both endpoint authorities.

## Names and DNS behavior

Canonical names are:

```text
<service-name>.<route-label>.svc.podmesh.internal.
```

`route-label` is lowercase unpadded base32 of the full 32-byte routing digest (52 characters).
Existing hexadecimal routing/ingress keys remain unchanged. A single-Service catalog also gets
`<route-label>.svc.podmesh.internal.`. Unique short service names and service/workload aliases are
available within the connector's authorized cache; ambiguous aliases are not resolved.

Internal A queries return a connector-local VIP in `198.18.0.0/24`. Addresses are persisted and
never reassigned within that identity/state directory; at most 128 distinct service mappings are
allocated. Startup refuses conflicting non-default IPv4 routes. Known live IPv4-only services
return NODATA for AAAA. Missing/expired/incomplete internal discovery returns SERVFAIL and never
contacts upstream DNS; an empty eventual cache does not establish NXDOMAIN.

Public forwarding returns CNAMEs rather than chasing them. A subsequent query therefore passes
through split DNS again, preventing public aliases into internal names from leaking upstream.
Request IDs/questions and upstream sources are checked, EDNS output is bounded, and there is no
claim of DNSSEC validation. Transfers/updates and unsupported operations are refused.

## Remaining acceptance and limitations

Rootless native images and Podman DNS/VIP/same-UID/upstream-mark behavior, proxy-offline availability,
replica expiry, ordinary updates and teardown have been exercised. Re-run serially with a running
Podman API socket and the native images:

```sh
./deploy/build_containers.sh
cargo test --locked -p podmesh-integration-tests --features podman-tests --test complete_rootless_stack -- --nocapture
cargo test --locked -p podmesh-integration-tests --features podman-tests --test podman_transparent_egress_test -- --nocapture
```

The namespace probe re-executes the Rust test binary using host `podman unshare` and `nsenter`; no
shell or diagnostic binary is installed in the scratch connector. Proxy grants are in memory in
this PoC: the test explicitly restores owner authorization after restarting proxies. The ordinary
uninterrupted-update regression runs before destructive outage checks; post-outage ingress recovery
is checked separately. Bare-host rootful deployment-manifest execution and kernel upstream TCP-fallback/fake-upstream privacy
acceptance are still pending (those forwarding behaviors are covered in-process).
Managed identity volumes, DNS/discovery-specific readiness/metrics, arbitrary application UDP,
synthetic IPv6 service transport, physical multi-host acceptance, autonomous rescheduling and VM
attestation are not delivered. Trusted-host and routing/firewall limitations still apply; tenant
bridges do not prevent proxy-mediated access to destinations the proxy can reach.

## VM/libkrun regression checks

The existing VM harness runs Podman inside independent Alpine guests. It requires Linux x86_64,
usable `/dev/kvm`, libkrun 1.19.6/libkrunfw 5.6.2, scoped root networking privileges, the musl
Rust target/linker, and the guest image built with `sh tests/kvm/build-image.sh`.

Rebuild guest binaries after source changes; do not reuse an older sidecar from `target/kvm/bin`.
The builder optimizes the debug binaries while retaining debug-build assertions: unoptimized
guest builds miss enrollment/discovery deadlines in the lean guests. Fixture images install
the connector at the same `/usr/local/bin/podmesh-sidecar` path as the production image.

Run these serially, not alongside other VM or workspace checks:

```sh
sh tests/kvm/build-binaries.sh
cargo test --locked -p podmesh-integration-tests --features kvm-tests --lib kvm::tests
cargo build --locked -p podmesh-integration-tests --features kvm-tests --bin kvm-spike
target/debug/kvm-spike probe target/kvm/agent.raw
target/debug/kvm-spike run target/kvm/agent.raw target/kvm/bin
cargo test --locked -p podmesh-integration-tests --features kvm-tests --test kvm_mesh \
  eight_vm_ -- --nocapture
```

The two-VM workload slice and eight-VM direct/forced-relay tests exercise normal enrollment and
DNS bootstrap along with ingress, transparent external egress, fixed-placement updates and
cleanup. Lifecycle checks use non-forced deletion so a failed delete cannot silently discard
the owner catalog. After fixture/bootstrap/enrollment and restart-readiness fixes, the
2026-10-18 two-VM probe/workload checks pass; both eight-VM modes pass twice consecutively.
Earlier failures remain recorded in the owning change's verification note, not counted as passes.

### Cross-VM DNS client/backend sample

The eight-VM tests automatically deploy
[`workload-dns-backend.yml`](../tests/kvm/workload-dns-backend.yml) and
[`workload-dns-client.yml`](../tests/kvm/workload-dns-client.yml) through normal owner control.
Both use the locally preloaded HTTP fixture, so no Internet image pull or diagnostic package is
needed. The backend returns its pod hostname; the client listens only on 8082, not backend port
8081. Each requests 300m CPU: with the harness's 1000m agents and 50% reservation ceiling, they
cannot co-locate. The test additionally asserts different agent VMs.

From the client application container the fixture resolves
`echo.<backend-route-id>.svc.podmesh.internal.:18081` with the ordinary libc resolver, verifies
only `198.18.0.x:18081` answers, and makes an HTTP request through the synthetic VIP. The Service
maps **18081 to backend targetPort 8081**. The exact remote pod-specific response must match;
HTTP 200 alone is insufficient. The harness prints the actual canonical name, VIP and agent pair.
The namespace here is the owner's identity, not an additional Kubernetes namespace boundary.

Run either scenario independently after building guests:

```sh
cargo test --locked -p podmesh-integration-tests --features kvm-tests --test kvm_mesh \
  eight_vm_direct_mesh -- --exact --nocapture
cargo test --locked -p podmesh-integration-tests --features kvm-tests --test kvm_mesh \
  eight_vm_forced_relay_mesh -- --exact --nocapture
```

Normal-network mode permits direct UDP; a passing run does not prove Iroh selected a direct path.
Forced-relay mode blocks inter-guest direct UDP, then temporarily blocks relay transport and
requires the remote HTTP request to fail and recover after restoration. Relay transport carries
encrypted sidecar-to-sidecar traffic, not application-proxy forwarding. Both modes retain the
existing ingress, external egress, update, scheduler restart, control-paused traffic and acknowledged
deletion checks. Sample pods must be absent after cleanup.

Bootstrap waits for a signed relay hint when relay transport is configured; this does not gate
ongoing DNS or seeded peer routing on a live proxy. Background enrollment still checks restarted
sidecars, while owner mutations wait boundedly behind enrollment and competing owner mutations
remain fail-fast.

For low workspace space, the builder accepts an external Cargo cache, for example
`CARGO_TARGET_DIR=/tmp/podmesh-kvm-build sh tests/kvm/build-binaries.sh`; final stripped guest
binaries still go to `target/kvm/bin`. Native checks can use `CARGO_INCREMENTAL=0` to avoid
regenerating a large incremental cache. These tests prove rootful Podman networking across
independent VMs on one host, not a native VM workload backend, physical multi-host operation,
global isolation or production readiness.

### Real PostgreSQL/frontend two-tier VM tests

The separately selectable two-tier scenarios use the actual
[`demo_two_tier_db.yml`](../deploy/demo_two_tier_db.yml) and
[`demo_two_tier_frontend.yml`](../deploy/demo_two_tier_frontend.yml), not the HTTP fixture as a
database substitute. They start the same eight-VM control/proxy topology, with agent guests
increased to **2 vCPUs/1024 MiB each** (11 vCPUs/4352 MiB total). Their agent capacity matches
those resources. The standard 50% reservation ceiling is unchanged: PostgreSQL plus injected
sidecar requires **600m CPU/384 MiB**, exceeding the lean profile's reservation ceiling.

Prepare the declared PostgreSQL image on the host, then save an archive. Guest tests only load
that archive; they do not depend on guest Internet access. Use fresh guest binaries after policy
or runtime changes; stale binaries can reject the sample's supported pod-ephemeral `emptyDir`
volumes. Demo database credentials are for disposable testing only.

```sh
podman pull docker.io/library/postgres:16-alpine
podman save --format docker-archive -o target/kvm/postgres.tar \
  docker.io/library/postgres:16-alpine
sh tests/kvm/build-binaries.sh
CARGO_INCREMENTAL=0 cargo test --locked -p podmesh-integration-tests \
  --features kvm-tests --test kvm_mesh two_tier_vm_ -- --nocapture
```

Alternatively set `PODMESH_KVM_POSTGRES_IMAGE` to an existing archive outside the workspace.
Individual selectors are `two_tier_vm_direct_mesh` and `two_tier_vm_forced_relay_mesh`
(use `-- --exact --nocapture`). The complete unfiltered `kvm_mesh` suite now requires the archive
and runs all four scenarios serially.

The two-tier tests verify anti-affinity in both owner placement and actual guest pods, resolve
`postgres-db.svc.podmesh.internal:5432` from the frontend container to synthetic VIPs, and require
the exact ingress body `DB_RESULT=podmesh-two-tier-postgres-ok`. This is a real PostgreSQL query
from the frontend on a different VM. A fresh SQL query must fail during the forced-relay block
and succeed after restoration, avoiding a previously buffered frontend response as the negative
control. Both workloads use non-forced deletion and must leave no sample pods.

On 2026-10-18 both modes passed twice with freshly rebuilt guests; the final confirmation run
took approximately **110s normal-network / 114s forced-relay**, including VM shutdown.
The two-tier scenarios test that sample instead of the lean profile's HTTP/update/restart
phases; retain the `eight_vm_` scenarios for those regressions. These tests do not add persistent
database storage, database failover, physical multi-host acceptance or tenant network isolation.
