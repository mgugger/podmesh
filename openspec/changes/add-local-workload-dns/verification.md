# Verification

## Delivered scope and acceptance boundary

Runtime-neutral connector control, endpoint-bound enrollment/renewal, signed Service catalogs,
workload-local DNS/discovery/direct peer TCP, deployment seed wiring and **single-host rootless
Podman DNS/kernel acceptance** are implemented. This is not completion of all tasks: managed volumes,
rootful/multi-host/VM acceptance and broader DNS readiness/metrics remain pending.

Reports contain public bound authority and discovery records, never private keys or relay tokens.
Self-signatures alone prove possession, not tenant enrollment. Standing delegated authority permits
owner-offline short-child renewal; finite parents cannot self-extend. Trusted-host/shared-network
and non-immediate-revocation limits remain.

## Real Podman verification — current implementation

Executed on Linux/aarch64, rootless Podman 6.1.4, one physical host, using a temporary owned API
service at `/run/user/1000/podman/podman.sock`. Tests were run **serially**, not alongside normal
workspace server tests. No host resolver or host nftables rules were changed.

- `./deploy/build_containers.sh`: **passed**, native static scheduler/agent/proxy/sidecar images.
  After runtime fixes, the sidecar was rebuilt incrementally from the same cached native build
  environment/dependency artifacts with locked source and installed over the scratch sidecar image.
  Final tested sidecar ID: `3f41dd251aa7f6479baa09dda13bbdb9285a61f07ce080c3631c0481ece3d2f8`.
- `cargo test --locked -p podmesh-integration-tests --features podman-tests --test complete_rootless_stack -j 2 -- --exact complete_rootless_stack_serves_ingress --nocapture`:
  **passed**, one real test, **341.23 seconds**. The standalone namespace helper is filtered out in
  the parent process and explicitly re-executed inside workload network namespaces.
- `cargo test --locked -p podmesh-integration-tests --features podman-tests --test podman_transparent_egress_test -j 2 -- --nocapture`:
  **passed**, one test, **51.01 seconds**. Curl HTTP 200 and sidecar/proxy tunnel log patterns prove
  transparent external egress, not direct application bypass.

The full stack retains health/metrics, distinct three-agent placement and connector identity,
existing ingress, single-replica update/reconciliation, ordinary uninterrupted sequential update
and deletion/guard cleanup checks. A separate curl-client workload has no nginx on loopback.
The Rust namespace probe checks:

1. Same-UID UDP and TCP DNS through redirected port 53 return the same `198.18.0.x` VIP.
2. Internal AAAA returns NODATA, while `podmesh-control` resolves through the captured runtime
   upstream, exercising socket-mark loop avoidance.
3. Ordinary TCP to VIP **service port 18081** reaches remote nginx **targetPort 80** across Iroh.
   The source workload cannot satisfy the response via local application fallback.
4. All three proxies are stopped; internal DNS/TCP still succeed with no owner deployment commands.
5. One workload replica is stopped; setup retries still reach siblings.
6. After **125 seconds**, beyond its maximum discovery lease, DNS/peer TCP still work with both
   that replica and all proxies offline.
7. Replica/proxies restart; in-memory owner proxy grants are explicitly restored, ingress recovers,
   and workloads/infrastructure are deleted.

The probe re-executes the Rust test binary with host `podman unshare`/`nsenter`; it does not install
shell/probe binaries in scratch images or download diagnostic packages. The ordinary continuous
update assertion runs before destructive outage checks; post-outage ingress recovery is separate.
No uninterrupted rolling-update claim is made for an immediately restored/degraded proxy topology.

### Failures found and fixed

- IPv4 DNS/VIP matching expressions were appended **after DNAT**. Moved all filters before NAT
  register loads/action; added an expression-order unit regression and exercised real interception.
- Authenticated older anti-entropy records aborted batches. Peer merges now validate signatures,
  catalog/generation/keys and ignore older sequences without regressing state. Strict direct admission
  still refuses regression, and the new regression test also refuses forged older records.
- Repeated QUIC teardown/re-handshake failed during longer relay/proxy outage. Increasing timeouts
  alone did not fix it. Peer control/application streams now reuse a bounded transport pool and are
  authenticated independently. Incoming sessions/operations, idle shutdown and setup retry deadlines
  remain bounded; tunnel teardown does not close unrelated streams. Long-outage Podman checks pass.
- Asynchronous enrollment meant a successful deployment response was not replacement readiness.
  `podctl` now records confirmed placement then waits for the replacement's live enrolled discovery
  record at the requested revision before replacing another replica. Normal uninterrupted updates pass.
- Restarted proxies intentionally lose in-memory grants. The outage test restores owner authorization
  before ingress checks instead of claiming durable grant behavior.

Early runs failed and are retained as diagnostics, not counted as passes. Logs and exit markers are
under `/home/manuel/.cache/podmesh-verification/`: `podman-dns-stack-v9.log/.exit`,
`podman-dns-egress.log/.exit`, image build/rebuild logs and the earlier failed runs.
All test workload/infrastructure containers and the temporary rebuild container were removed;
the temporary owned Podman API process was stopped. Tested images/build cache are retained.

## Normal regression and source checks

- Latest focused DNS/peer/production tests: **passed**, seven DNS, one three-peer, one production path.
  Deterministic forwarding tests cover A/AAAA/CNAME/NXDOMAIN, bounds, cache, mismatched IDs,
  truncated UDP/TCP fallback, internal privacy, malformed input, partial TCP deadlines and shutdown.
  Cache tests cover signatures, aliases, full-digest labels, stable persisted VIP replacement and
  authenticated stale merge. Three actual loopback Iroh endpoints prove sparse discovery, remote
  responses, retry, expiry and foreign-tenant wire refusal.
- `production_dns` starts the normal sidecar, installs a signed seed over connector control, serves
  DNS, persists VIP state and routes remote TCP with no live proxy; kernel rules are disabled there.
- Final `cargo test --locked --workspace -j 2`: **passed**, **540 tests**, zero failures/ignored,
  across 63 unit/integration/doc-test result groups. Exit zero: `podman-dns-workspace.exit`.
  Real Podman tests above are separate, not counted in this normal-workspace total.
- `cargo clippy --locked --workspace --all-targets --features podmesh-integration-tests/podman-tests -j 2 -- -D warnings`:
  **passed**, including feature-gated Podman tests, on the current implementation.
- `cargo fmt --all --check`, `git diff --check` and
  `openspec validate add-local-workload-dns --strict --no-interactive`: **passed** after documentation updates.

## Historical connector/enrollment checks

The initial connector increment passed 518 normal tests and historical real rootless stack/egress
checks in 84.82/55.60 seconds. Subsequent enrollment checks and updated-image stack/egress passed in
121.18/61.82 seconds after moving issuance off the deployment response path. Those historical runs
were not DNS acceptance; the current executed tests above supersede that limitation.

## Remaining limitations

Pending: rootful execution, separate-host/NAT-relay behavior, IPv6 kernel DNS probing, deterministic
fake-upstream privacy and upstream TCP-fallback acceptance inside containers, live second-owner
container isolation, managed identity volumes and broader DNS/discovery-specific metrics/readiness.
In-process tests cover forwarding privacy/TCP fallback and foreign-tenant peer refusal. Writable-layer
keys/state survive normal restart, not recreation; explicit owner generation is required for lost keys.
No arbitrary application UDP, synthetic IPv6 service transport, autonomous rescheduling, VM backend
or hardware attestation is claimed. This change remains open for review/unimplemented acceptance.

Cargo reports existing `proc-macro-error2` future-incompatibility warnings; the image's newer Rust
also warns about existing MockRuntime `fetch_update`. Historical global OpenSpec strict validation
failed on unrelated requirement bodies exceeding 500 characters; unrelated specs were not rewritten.
