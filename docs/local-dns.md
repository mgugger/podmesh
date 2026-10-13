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
cargo test --locked -p podmesh-sidecar --test local_dns --test local_peer_dns --test production_dns
```

Fixtures use production catalog, credential, record and envelope validators, not an authorization
bypass. They start no containers, do not modify host DNS/routes/nftables, and need no Internet DNS.

- `local_dns`: deterministic upstream A/AAAA/CNAME/NXDOMAIN, cache, timeout, mismatched IDs,
  truncated UDP/TCP fallback, internal-query privacy, admission limits and shutdown; signed-record
  security, base32 properties and persisted VIP restoration/catalog replacement.
- `local_peer_dns`: three real loopback Iroh endpoints, sparse introduction, paginated discovery,
  replica-specific remote TCP responses, Service targetPort translation, setup-only retry, expiry
  and foreign-tenant wire refusal.
- `production_dns`: normal sidecar startup, signed seed installation through connector control,
  local DNS, durable mapping and internal HTTP CONNECT dispatch over real Iroh. Its remote
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
  a new owner intent. Agent control/runtime exec mediates seed delivery; machine peers do not become
  discovery members. Bootstrap failure retains recorded placements so `podctl apply` can retry.
  Sequential updates wait for each replacement's live enrolled record at the requested revision
  before replacing the next replica; a deployment response itself is not enrollment readiness.
- `workload_network.rs` refreshes leases every ten seconds and on authority changes, synchronizing
  up to four neighbors. Records expire within 120 seconds and never outlive their credential.
  Stale signed hints can bootstrap synchronization only, never authorize application traffic.
  Up to 32 outbound peer connections and 32 incoming sessions are retained with bounded stream
  admission and idle shutdown. Every stream still carries fresh signed, currently verified authority.
  Valid older anti-entropy snapshots are ignored without aborting batches or regressing state.
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
is checked separately. Rootful execution and kernel upstream TCP-fallback/fake-upstream privacy
acceptance are still pending (those forwarding behaviors are covered in-process).
Managed identity volumes, DNS/discovery-specific readiness/metrics, arbitrary application UDP,
synthetic IPv6 service transport, multi-host/NAT-relay acceptance, autonomous rescheduling and VM
attestation are not delivered. Existing trusted-host/shared-network limitations still apply.
