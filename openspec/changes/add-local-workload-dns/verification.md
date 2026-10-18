# Verification

## Real two-tier VM follow-up - 2026-10-18

The user approved adding real PostgreSQL/frontend VM tests after confirming the existing
two-tier tests covered mock workloads/host Podman, not guests. No user acceptance is inferred.

- Existing `podctl_deployment_flow` two-tier selector: **passed, 12.81s**. This is mock-runtime
  control/placement/seed evidence only, not database execution.
- Added `two_tier_vm_direct_mesh` and `two_tier_vm_forced_relay_mesh`, reusing the eight-VM
  topology with the actual two-tier deployment manifests. They load an explicit PostgreSQL image
  archive and run this sample instead of the lean profile's HTTP/update/restart phases.
- Initial executions failed admission: database plus injected sidecar is **600m CPU/384 MiB**,
  above the lean profile's **500m/256 MiB** reservation ceiling. The dedicated profile now gives
  each agent **2 vCPUs/1024 MiB** with matching advertised capacity; 50% reservation policy stays
  unchanged. Total is **11 vCPUs/4352 MiB**, enforced in memory preflight/unit assertions.
- Next runs exposed stale copied guest binaries rejecting the current sample's `emptyDir` volumes.
  Rebuilt all service binaries with the existing optimized musl builder and external Cargo cache;
  no policy bypass or volume workaround was added. Fresh guests accept the current source policy.
- PostgreSQL archive: `docker.io/library/postgres:16-alpine`, image ID
  `81bd698b4594e751a3269e4dcd3e03a4a0ec0daf7b72e7aa1abd43cce9887542`.
  Prepared with an isolated Podman graph/run/tmp directory because shared host Podman has stale
  boot state. Shared runtime state was not deleted or changed. The reusable archive is retained
  at `target/kvm/postgres.tar`; tests also support `PODMESH_KVM_POSTGRES_IMAGE`.
- Both real two-tier scenarios **passed** after rebuilding. A confirmation run using the default
  archive path also **passed, 2 tests in 225.81s**, normal-network **109.77s** and forced-relay
  **114.30s** including shutdown. Both resolved the configured alias to **198.18.0.2:5432**,
  asserted distinct placement/actual VMs, returned exact ingress
  `DB_RESULT=podmesh-two-tier-postgres-ok`, and removed both pods after non-forced deletion.
  Forced mode additionally required a fresh `psql` command to fail while relay transport was
  blocked, then verified SQL and ingress recovery after restoration.
- Wider regression run: KVM units **4 passed**, original normal-network mode passed, both new
  two-tier modes passed, but original forced mode hit a transient immediate local HTTP-fixture
  read failure. The backend identity check now derives its expected hostname from actual container
  inspection and waits for the exact local HTTP body; no response/placement assertion is weakened.
  Original forced-relay isolated rerun then **passed, 193.09s**. An intervening combined run was
  interrupted after completed normal-network/two-tier-direct passes; not counted as a full pass.
- Targeted feature-inclusive Clippy **passed** with the previously documented
  `-A deprecated -A clippy::double_must_use` baseline exceptions after `-D warnings`.
- Logs in this session's `files/`: `kvm-two-tier-existing.log`, `kvm-two-tier-first.log`,
  `kvm-two-tier-sized.log`, `kvm-two-tier-guest-rebuild.log`, `kvm-two-tier-rebuilt.log`,
  `kvm-two-tier-confirm.log` and `kvm-forced-readiness-final.log`. Earlier failures remain recorded,
  not converted into passes.
- Final touched-file formatting, `git diff --check` and owning-change strict validation **pass**.
  All-project strict validation remains **3 passed/13 failed** on the documented unrelated baseline.
  No VM workers or owned `pmh-` links remain. Isolated temporary host Podman storage was removed;
  the 284 MiB reusable PostgreSQL archive and copied guest binaries remain. Workspace has **7 GiB
  free**. Owning change now has **70/77 tasks complete**; unrelated pending acceptance remains open.
- VM-hosted rootful Podman on one physical host only. Direct-permitted mode does not prove the
  selected Iroh path. Storage is disposable pod-ephemeral `emptyDir`; no database durability,
  rescheduling, failover, physical multi-host readiness or isolation claim.

## Final VM DNS sample verification - 2026-10-18

This section supersedes the incomplete VM investigation below; it does not complete unrelated
pending DNS acceptance tasks or record user acceptance.

### Confirmed defects and fixes

- Before adding the sample, non-forced original mesh scenarios passed twice: direct **96.5/96.8s**,
  forced-relay **123.0/129.9s**. The separate client/backend sample then exposed problems hidden
  by same-host/private-network tests.
- Initial service records contained only overlapping guest Podman addresses while fresh endpoint
  reports already included relay hints. Sidecars now refresh on endpoint-address changes and
  withhold bootstrap service records lacking a configured relay hint. Final VM reports show both
  current and published relay hints. No endpoint is invented or unsigned record modified.
- The sample succeeded, but a later update explicitly failed with agent `workload is busy`.
  Enrollment and owner mutation now have distinct lock ownership. Owner requests wait boundedly
  only behind enrollment, reverify authority after waiting and reread current state. Competing owner
  mutations remain fail-fast; accounting, key binding and restart inspections remain intact.
- A later forced-relay scenario completed DNS/update but failed post-scheduler-restart status with
  a transient 404. HTTP health had been mistaken for recovered mesh state. The harness now polls
  for all three replica statuses with a 45-second deadline and explicit pending-error diagnostics.

### Executed VM results

- Optimized musl guest rebuild passed using an external `/tmp` Cargo target directory; the builder
  now respects `CARGO_TARGET_DIR` when copying binaries. The fixture lives at the production
  `/usr/local/bin/podmesh-sidecar` path. No deadlines or authorization were relaxed.
- KVM harness unit tests: **4 passed**.
- Final two-VM kernel/Podman probe: **passed**. Final two-VM workload slice: **passed, 46.5s**,
  including ingress, transparent egress, scheduler replacement/identity, logs and deletion.
- Final combined eight-VM suite: **2 passed, 317.43s** (normal-network **142.83s**, forced-relay
  **173.84s**, including shutdown). Confirmation run: **2 passed, 315.64s**.
- Both runs used ordinary application DNS, required synthetic VIPs, mapped Service **18081** to
  backend **8081**, checked distinct agent VMs and the exact backend pod hostname, and proved the
  client had no listener on 8081. Forced-relay request failure/restoration and acknowledged cleanup
  passed. Existing ingress/egress, sequential update, restart and control-paused traffic checks passed.
- One earlier run was interrupted after sample/egress success; it is not counted as a pass.
  Another completed normal-network mode but failed the forced-relay restart readiness check before
  that check was corrected. Initial sample and historical relay failures remain diagnostic evidence.

### Focused checks and review boundary

- Final agent library: **53 passed**, including enrollment waiting, bounded/cancellation-safe
  acquisition, the existing fail-fast concurrent creation regression and authority-after-wait expiry.
  The latter crosses expiry plus the protocol clock-skew allowance without mutation.
- Sidecar library **25**, local DNS **7**, peer DNS **1** tests passed. Production DNS test passed
  both direct-only and configured-but-unavailable relay cases, preserving seeded DNS/TCP operation
  while relay-dependent bootstrap remains pending.
- Focused sample-manifest regression passed using the production Service compiler.
- Detailed logs: this session's `files/kvm-dns-full-final.log`, `kvm-dns-full-confirm.log`,
  `kvm-two-vm-dns-final.log`, `kvm-agent-contention.log`, `kvm-agent-expiry.log` and
  `kvm-sidecar-readiness-repeat.log`.
- Strict scoped Clippy remains blocked by pre-existing generated async-trait `double_must_use`
  warnings. The affected-package/all-targets command with `--features kvm-tests` **passes** with
  only `-A deprecated -A clippy::double_must_use` after `-D warnings`; not a strict-lint pass.
- Touched-file `rustfmt --check`, builder shell syntax and `git diff --check`: **passed**.
  Owning-change strict OpenSpec validation: **passed**. All-project strict validation still
  reports **3 passed, 13 failed**, including unrelated long requirements, a missing scenario in
  the source traffic-plane spec and the pre-existing change with no deltas. No unrelated cleanup.
- Disk cleanup removed only regenerable cross-build/incremental caches, initially freeing about
  12 GiB. Native incremental cache regenerated during validation and was removed again; subsequent
  checks use `CARGO_INCREMENTAL=0`. Source, images and copied guest binaries are retained. Final
  workspace free space is **8.3 GiB** after checks; no VM workers or `pmh-` host links remain.
- Normal-network mode permits direct UDP but does not measure the selected Iroh path. Forced mode
  proves relay-dependent cross-VM transport. All VMs share one host; physical multi-host,
  IPv6-kernel DNS, managed identity volumes and global isolation remain unverified/out of scope.

## VM/libkrun regression investigation - 2026-10-18

- Host: Linux x86_64 with usable `/dev/kvm`, libkrun/libkrunfw, musl target/linker and the
  existing Alpine guest base. Rebuilt all four service binaries and the HTTP fixture from
  current source with `sh tests/kvm/build-binaries.sh`; build passed.
- `cargo test --locked -p podmesh-integration-tests --features kvm-tests --lib kvm::tests -- --nocapture`:
  **passed**, four harness unit tests.
- `cargo build --locked -p podmesh-integration-tests --features kvm-tests --bin kvm-spike`:
  **passed**. The existing agent `Atomic::fetch_update` deprecation warning remains.
- `target/debug/kvm-spike probe target/kvm/agent.raw`: **passed**, both guests report Linux
  6.12.109, ext4 and Podman cgroup v2/overlay; namespace/nftables probes pass.
- Initial two-VM workload slice and both tests in
  `cargo test --locked -p podmesh-integration-tests --features kvm-tests --test kvm_mesh -- --nocapture`:
  **failed** at DNS peer bootstrap/connector readiness, before existing traffic/lifecycle
  acceptance. These are actual failed executions, not passes or missing-runtime skips.
- Found a stale fixture contract: both VM OCI preload scripts installed the sidecar at
  `/podmesh-sidecar`, while runtime enrollment executes `/usr/local/bin/podmesh-sidecar`.
  Aligned both fixture images and their entrypoints/help check with the production image.
  No fallback binary path or enrollment bypass was added.
- After the path fix, unoptimized two-VM builds still miss connector inspection/traffic
  deadlines. `CARGO_PROFILE_DEV_OPT_LEVEL=3 sh tests/kvm/build-binaries.sh` followed by the
  same two-VM workload command **passed in 55.0 seconds**, without changing runtime deadlines
  or authorization. The guest builder now selects that optimization level by default, retaining
  debug-build assertions. Production images already use optimized builds. This establishes a
  build-sensitive deadline failure, not a measured CPU quota or mutex-deadlock root cause.
- Eight-VM direct mode **passed twice**, in **96.0** and **100.4 seconds**, including distinct
  placements, both proxy ingress/egress paths, update, restart, control-paused traffic and cleanup.
- Eight-VM forced-relay mode is **intermittent, not reliably green**:
  - First optimized combined run failed at capacity discovery (503 deadline); scheduler diagnostics
    report gossip dial timeouts.
  - Isolated rerun **passed in 124.5 seconds**, including negative control and 2812 direct UDP drops.
  - Final combined rerun passed direct mode but failed forced-relay mode because a single-proxy
    egress workload remained after deletion.
  - Found that the harness used `force=true`, which can discard catalog state even when deletion
    fails. All three harness deletions now use `force=false`, and egress deletion adds workload
    context. The stronger isolated forced-relay run **passed in 124.6 seconds**, with 2917 UDP drops.
    This does not establish that the intermittent underlying failures are fixed.
- Background enrollment and owner deletion both acquire the same non-waiting workload mutation
  guard; `mutation` can return `workload is busy`. This is a possible contention mechanism, not
  the confirmed error from the failed deletion (the earlier force flag hid that error). No
  speculative runtime concurrency or scheduler fix was made. Further diagnosis is required
  before calling the baseline fully working or starting the requested DNS sample.
- Focused regressions: `cargo test --locked -p podctl --lib`: **29 passed**;
  KVM harness units: **4 passed**; `cargo test --locked -p podmesh-integration-tests --test
  podctl_deployment_flow`: **8 passed**.
- Strict scoped Clippy fails on existing agent `Atomic::fetch_update` deprecation and generated
  async-trait `double_must_use` warnings in agent/proxy traits. The same scoped Clippy command
  passes with only `-A deprecated -A clippy::double_must_use` after `-D warnings`; this is not
  a strict-lint pass.
- `openspec validate --all --strict --no-interactive`: **3 passed, 13 failed**. The owning
  `add-local-workload-dns` change passes; unrelated length warnings and the pre-existing
  `tenant-scoped-service-resolution` change with no spec deltas prevent all-project success.
- At the user's request, removed only the resolved musl cross-build and native incremental
  cache directories, recovering approximately **12 GiB** (96% to 75% workspace utilization,
  approximately 15 GiB free immediately afterward). Kept guest image/copied binaries and
  source changes; the optimized guests remain the tested inputs.
- Detailed build/scenario logs are retained in this Copilot session's `files/` directory.
  All completed attempts invoke the existing owned-worker/network teardown path.
- Final touched-file rustfmt checks, shell syntax, `git diff --check` and owning-change strict
  validation pass. No VM workers or `pmh-` host links remain. One stale namespace entry,
  `pmk-t80Tmx`, predates this task (created 2026-10-09); left untouched in the shared environment.
  Workspace has approximately 13 GiB free after final native checks.

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

Pending beyond the VM evidence above: bare-host rootful deployment-manifest execution, physical
multi-host behavior, IPv6 kernel DNS probing, deterministic
fake-upstream privacy and upstream TCP-fallback acceptance inside containers, live second-owner
container isolation, managed identity volumes and broader DNS/discovery-specific metrics/readiness.
In-process tests cover forwarding privacy/TCP fallback and foreign-tenant peer refusal. Writable-layer
keys/state survive normal restart, not recreation; explicit owner generation is required for lost keys.
No arbitrary application UDP, synthetic IPv6 service transport, autonomous rescheduling, VM backend
or hardware attestation is claimed. This change remains open for review/unimplemented acceptance.

Cargo reports existing `proc-macro-error2` future-incompatibility warnings; the image's newer Rust
also warns about existing MockRuntime `fetch_update`. Historical global OpenSpec strict validation
failed on unrelated requirement bodies exceeding 500 characters; unrelated specs were not rewritten.
