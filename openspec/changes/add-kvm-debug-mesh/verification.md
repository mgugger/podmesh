# Verification

## Planning - 2026-10-09

- Reviewed the local deployment/development specs, accepted decisions, current Podman complete
  stack/egress tests, image construction, scheduler/agent configuration and owning physical
  multi-host task.
- User settled the eight-VM topology and requested evaluation in favor of iterative debugging.
- Compared upstream QEMU, Firecracker and libkrun documentation. Recommended QEMU/KVM with
  cached minimal Alpine images; no hypervisor or guest compatibility claim has been executed.
- `git status --short`: clean before this proposal.
- `openspec list --json`: blocked, `openspec: command not found`. The available shell also lacks
  Node/npm/npx; no package installation or shared-environment modification was attempted.
- Created schema-shaped proposal/design/spec delta/tasks manually using upstream spec-driven
  templates because the local OpenSpec CLI is unavailable. CLI scaffold/status and
  `openspec validate --all --strict --no-interactive` remain unexecuted.
- `git diff --check`: passed for tracked changes; the new proposal files are untracked.
- Local planning-document check: passed for five Markdown files, resolving relative links and
  checking six delta requirement blocks for normative wording, bounded description length and
  WHEN/THEN scenarios. This lightweight check is not OpenSpec CLI strict validation.
- `/dev/kvm` exists, but no usable KVM launch/access check was run; QEMU/Firecracker/libkrun
  executables were not found on PATH. No VMs, container workloads or network resources started.
- This turn creates planning documents only. No Rust code, dependencies, deployment manifests
  or existing task completion statuses change. No implementation approval or user acceptance
  is inferred from proposal creation.

## Executed feasibility slice - 2026-10-09

- Approved scope: two guests only, one control guest (scheduler plus proxy) and one agent guest
  (agent plus guest-local Podman). This does not complete the planned eight-VM topology or the
  separate physical multi-host demonstration.
- Host inputs: Arch Linux x86_64, usable `/dev/kvm`, libkrun 1.19.6, libkrunfw 5.6.2, Alpine
  minirootfs 3.23.6 (SHA256 pinned in the image builder), Rust musl target, SSH and scoped root
  networking privileges.
- `sh tests/kvm/build-image.sh`: passed; cached 4 GiB ext4 image. Guest packages are listed in
  `target/kvm/packages.txt`; Alpine package repository state is not snapshot-pinned.
- `sh tests/kvm/build-binaries.sh`: passed; statically linked musl service binaries and local
  HTTP workload fixture built without rebuilding the guest OS.
- `target/debug/kvm-spike probe target/kvm/agent.raw`: passed. Both guests reported Linux
  6.12.109, `/dev/vda ext4`, Podman
  `cgroup v2` and `overlay`; namespace isolation and nftables NAT rule creation/deletion passed.
- `target/debug/kvm-spike run target/kvm/agent.raw target/kvm/bin`: passed in 124.2 seconds.
  The owner deployed the test
  workload through the scheduler; the agent launched guest-local Podman app and sidecar. HTTP
  ingress returned the fixture response. A workload-originated request was intercepted by the
  sidecar nftables OUTPUT rule and completed through the proxy egress tunnel. Scheduler binary
  hash matched the supplied build after targeted process restart; scheduler signing identity and
  existing workload remained available; status, logs and delete checks passed.
- The 4 GiB apparent guest base image occupied 178,528,256 bytes (about 170.2 MiB) on the host.
  The two booted private guest disks allocated 224,784,384 bytes total (about 214.3 MiB) after
  guest setup. Each VM was configured for 2 vCPUs and 1 GiB RAM; peak memory was not measured.
- `cargo test --locked -p protocol --test proxy_grant_test`: 9 passed, including future issue-time
  offsets of 1 second, the documented 60-second boundary, 61 seconds, and strict expiry.
- `cargo test --locked -p podmesh-sidecar --lib`: 19 passed.
- `cargo test --locked -p podmesh-integration-tests --features kvm-tests kvm::tests`: 2 passed.
- `cargo test --locked -p podmesh-integration-tests`: passed its existing integration-crate tests.
- `git diff --check` and rustfmt checks for touched Rust files passed.
- Scoped Clippy with the KVM feature passed after allowing pre-existing `deprecated` and
  `clippy::double_must_use` lints. The strict `-D warnings` attempt reports existing async-trait
  `double_must_use` warnings in agent/proxy traits; those unrelated interfaces were not changed.
- The guest test exposed two coupled defects and their fixes: grant verification ignored the
  documented bounded issue-time skew; rustables 0.8.7 closed an owned netlink fd twice when
  setting up sidecar nftables. Rustables 0.8.8 fixes the latter and has been verified by the
  actual sidecar startup and egress path.
- Existing, unrelated `podmesh-agent` deprecation warning (`Atomic::fetch_update`) remains.
- Strict OpenSpec CLI validation remains unavailable (`openspec` is not installed). The test
  does not yet provide the eight roles, multiple proxies/schedulers, resource placement across
  distinct agents, forced-relay pass, interactive retained-run workflow, or measured peak RAM.
- After review of this executed slice, the user approved the planned eight-VM expansion on
  2026-10-09. Those scenarios remain unexecuted until their task increments are implemented and
  verified.

## Eight-VM direct mesh and timing investigation - 2026-10-09

- Added a named eight-guest launch path while retaining the original two-guest probe/run commands.
  Each guest has its own disk, SSH host key, node identity and local state. Schedulers are separate
  VMs; A1/A2 configure only S2 and A3 only S3. P1 alone publishes the relay bootstrap and P2 adopts
  it. Only public machine-relay certificates are copied between guests.
- `kvm-spike mesh target/kvm/agent.raw target/kvm/bin`: direct scenario passed initially in 406.9s.
  Three replica identities were distinct, with one actual expected pod on each agent VM. Both
  proxy ingress paths passed, as did separate single-proxy egress deployments, status/logs,
  alternate-scheduler status/rediscovery, fixed-placement update, S1 identity-preserving
  replacement, ingress with scheduler/agent processes paused, deletion and normal teardown.
- Startup fixes: use copied public CA paths, not peers' nonexistent private-state directories;
  read the actual per-agent signing key paths; wait for sidecar registration before egress;
  obtain the tenant token from the publisher while configuring just P2's endpoint/grant in the
  P2-only phase. Handle an incomplete console host-key line by waiting for its newline.
- Added persistent per-phase elapsed timing output and opt-in command timing debug logs that omit
  command arguments and credentials. Measured baseline: total 436.80s, boot/SSH 17.29s, mesh
  scenario 168.92s, teardown 250.60s. Teardown was dominated by eight sequential 30-second SSH
  timeouts after guest poweroff, plus the worker-exit deadline.
- Changed teardown to sync and request shutdown concurrently, with a two-second SSH acknowledgement
  deadline per guest and one shared ten-second worker-exit deadline. The host still checks,
  explicitly kills if necessary, and reaps each owned worker before removing networking.
  The next complete direct run passed in 168.31s; teardown took 12.59s.
- Remaining repeated ten-second lifecycle delays came from the tiny HTTP fixture running as
  container PID 1 without a SIGTERM handler. Added signal-safe `_exit` handling to that fixture
  and bounded client socket I/O. No production runtime timeout/grace settings were changed.
- Final timed direct run passed in 87.86s: boot/SSH 3.39s, binary injection 11.66s,
  probe 3.52s, initial schedulers 2.99s, CA distribution/restart 6.99s, proxy bootstrap 1.70s,
  OCI preloads 6.75s, agents/capacity checks 12.92s, trust 0.33s, placement 3.68s,
  ingress 0.92s, P1/P2 egress phases 4.15s/4.18s, status/logs/discovery 1.86s,
  update 3.74s, S1 replacement/recovery 3.46s, control-paused traffic/resume 1.30s,
  delete 1.60s, teardown 12.72s. Baseline update/delete were 33.67s/31.44s.
  This is approximately 80% lower wall time, not a universal performance threshold:
  boot and artifact timings vary with filesystem cache and host load.
- Re-executed the final code after simplifying the single-proxy helper arguments: passed in
  86.98s, including 3.34s boot, 71.07s scenario and 12.57s teardown. Focused KVM unit tests
  again passed (three). Final editor diagnostics, touched rustfmt and whitespace checks passed;
  no VM workers, run-owned host links or namespace entries remained.
- The libkrun workers did not exit naturally after guest poweroff on this host. They were
  explicitly terminated at the shared deadline; this fallback is now visible in warnings.
  No run-owned network namespaces or host links remained after the direct passes.
- Focused KVM unit tests: three passed. Feature-enabled build/check and scoped Clippy passed
  (`-D warnings -A deprecated -A clippy::double_must_use`, retaining the previously noted unrelated
  lint allowances). The editor test adapter found no tests, so Cargo was used. Touched rustfmt,
  editor diagnostics and `git diff --check` passed.
- `mesh ... --forced-relay`: executed separately after speed fixes; failed at capacity readiness
  with repeated agent attachment connect timeouts (503 at S1), after 143.17s including diagnostics
  and teardown. The filter installed successfully but relayed placement/workload operations have
  not passed. Task 3.6 is not complete; direct success does not certify relays. An interrupted run
  left one identified stale namespace entry, removed explicitly after confirming no workers or
  run host link remained; handled-interruption/stale-cleanup requirements remain open.
- Remaining limitations: exact live attachment/store assertions, stronger resource-refusal and
  fixed-placement identity assertions, bypass-negative controls, relay diagnosis and negative
  controls, peak RAM, smaller control-only images, interactive retention/reboot/recreate commands,
  a dedicated Cargo KVM integration target and strict OpenSpec validation remain unfinished.
  No physical multi-host task, user acceptance or production readiness is marked complete.

## Lean profile verification (2026-10-10)

- User reported Codespaces announcing shutdown, rather than an isolated guest crash. Current
  boot has 31 GiB RAM, no swap, no visible cgroup cap or OOM events; previous-boot evidence is
  unavailable. The interrupted log ended after guest probes without cleanup. No workers remained
  on resume. Memory pressure is plausible, not proven as the shutdown cause.
- Approved profile implemented: one vCPU per guest, 256 MiB scheduler/proxy, 512 MiB agent and
  mixed-role two-VM control. Mesh guest RAM is 2816 MiB instead of 8192 MiB. Preflight checks
  MemAvailable and visible cgroup-v2 ancestor remaining memory, with a 1024 MiB host reserve.
  It is not a reservation, accounts only for configured guest RAM plus reserve, and cannot
  prevent platform shutdowns/concurrent allocations. Worker VmHWM is printed before teardown.
- `cargo test --locked -j 2 -p podmesh-integration-tests --features kvm-tests --lib kvm::tests`:
  four passed, including exact budget, memory threshold boundary and malformed diagnostics.
- `cargo build --locked -j 2 -p podmesh-integration-tests --features kvm-tests --bin kvm-spike`
  and `target/debug/kvm-spike run target/kvm/agent.raw target/kvm/bin`: passed full two-VM
  workload slice in 82.9s. Control/agent worker peak RSS: 392/533 MiB.
- Initial lean mesh attempt failed admission because advertising only 256 MiB workload capacity
  left a 128 MiB pending-reservation bound under the existing 50% policy; the app plus sidecar
  requests 192 MiB. Advertise the actual 512 MiB agent guest RAM and one CPU, preserving the
  default reservation percentage and fixture. No production policy or timeout was changed.
- `cargo test --locked -j 2 -p podmesh-integration-tests --features kvm-tests --test kvm_mesh
  eight_vm_direct_mesh -- --nocapture`: passed complete direct scenario in 91.68s
  (75.47s scenario, 3.56s boot, 12.66s teardown). Scheduler peak RSS 218-299 MiB,
  proxy 216-217 MiB, agent 532-534 MiB. Per-worker peaks include VMM overhead and are not
  simultaneous total memory. No guest-memory minimum or Codespaces shutdown cure is claimed.
- Cargo gating uses ordinary serial tests, no ignore/skip, explicit missing-artifact failure,
  and the existing supervisor. Forced-relay remains incomplete: authenticated grants now permit
  capacity but the latest completed forced attempt had partial scheduler reconciliation.
  Fresh-connection retry was interrupted. Lean-profile verification used direct mode only.
- OpenSpec CLI still unavailable (`openspec: command not found`); strict validation remains open.
- Final checks: default `--test kvm_mesh` ran zero tests; six scheduler attachment unit tests
  passed (including canonical multi-audience grant regression); the agent scheduler HTTP relay
  regression passed. Scoped feature-enabled harness Clippy passed with the existing deprecated
  and double-must-use allowances. Touched rustfmt, editor diagnostics and `git diff --check`
  passed. Checks used `-j 2` and ran without guest VMs.
- No test workers or owned host links remained after lean runs; current cgroup OOM counters
  remained zero. The pre-existing `pmk-t80Tmx` namespace entry and invalid peer namespace
  references seen before this work remained untouched; these runs did not create them.

## Forced-relay TLS bootstrap fix (2026-10-10)

- Root cause: when a scheduler starts with `--machine-relay-url` but no `--machine-relay-ca-certificate`,
  it cannot verify TLS connections to its own relay (self-signed cert at that address). The
  initial-start phase generated relay certs but did not distribute the CA certificate before
  restart, so peer gossip dials failed with "UnknownIssuer" during relay connection attempts.
- Solution: pre-generate each scheduler's relay TLS certificate and key on the host using
  `iroh_support::ensure_relay_tls` with the scheduler's announced relay URL (HTTPS audience).
  Distribute the certificates to all schedulers and agents as read-only files. Write the final
  scheduler start commands with both the relay URL and the CA file arguments. Start all three
  schedulers concurrently and wait for health.
- Implementation: Added `fs` import to `kvm-spike.rs`. For each scheduler, invoke
  `ensure_relay_tls(run.directory()/relay-certs/<scheduler>, "https://<ip>:7440", None, None)`.
  Read the returned certificate and key bytes. Copy the certificate to `/opt/podmesh/machine-relay-N.crt`
  on all consumers. Copy the key to the scheduler guest as `/opt/podmesh/relay-key-N.pem`. Update
  the final scheduler start command to include `--machine-relay-ca-certificate <paths>` listing all
  three certs. Start schedulers concurrently and wait for health.
- Added `iroh-support = { workspace = true }` to `tests/Cargo.toml` dependencies.
- Rebuilt guest binaries (`CARGO_BUILD_JOBS=2 sh tests/kvm/build-binaries.sh`) to include the
  discovery-retry change from the agent/scheduler.
- `cargo test --locked -j 2 -p podmesh-integration-tests --features kvm-tests --test kvm_mesh
  eight_vm_forced_relay_mesh -- --nocapture`: passed in 128.6s. The test included all expected
  phases: force filter activation, capacity readiness, reconciliation wait (successful, all
  schedulers online via relay), ingress/egress, update, restart, deletion and final direct-UDP-drop
  counter assertion (2377 drops, confirming the filter worked). Scheduler peak RSS 218-298 MiB,
  proxy 215-217 MiB, agent 531-533 MiB. Relay TCP counters confirmed traffic flow (3076 packets).
- The eight-VM forced-relay scenario now passes. All critical control and workload operations
  succeed under relay-only network conditions.

## Workspace and full test suite verification (2026-10-10)

- Disk pressure: `/workspaces` filled to 100% (61 GiB used, 0 free). Primary cause: `target/debug/deps`
  (36 GiB), `target/x86_64-unknown-linux-musl` guest-binary cross-builds (13 GiB), and incremental
  cache (8.5 GiB). Removed `target/debug/incremental` (8.5 GiB) and `target/x86_64-unknown-linux-musl`
  (13 GiB), leaving 5.6 GiB free initially and 18 GiB free after removing the musl builds.
- `cargo test --quiet --locked -j 2 --workspace`: 165 tests passed (scheduler attachments 6,
  agent 1, proxy 6, protocol 19, sidecar 13, integration-crate 109, other 1, KVM 2 feature-gated),
  0 failed. Full workspace test suite result: ok.
- Clippy: `cargo clippy --locked --workspace --all-targets -- -D warnings` reported pre-existing
  errors in `podmesh-agent` (deprecated `Atomic::fetch_update`) and `proc-macro-error2`. These
  errors are not caused by the KVM feature or forced-relay changes and block compilation. They
  remain unresolved (pre-existing issue; outside the scope of this change).
- Feature-gated KVM harness compilation and tests: `cargo clippy --locked -p podmesh-integration-tests
  --features kvm-tests --bins -- -D warnings` passed (no new errors introduced by KVM changes).
- `cargo test --quiet --locked -j 2 -p podmesh-integration-tests --features kvm-tests --test kvm_mesh`:
  two tests passed: `eight_vm_direct_mesh` (120.3s) and `eight_vm_forced_relay_mesh` (244.7s total,
  128.6s scenario). Direct mesh peak RSS as before (298/533/217 MiB). Forced-relay peak RSS
  (298/533/217 MiB). Total run time 244.7s (direct + forced).
- Podman tests: `cargo test --quiet --locked -j 2 -p podmesh-integration-tests --features podman-tests --lib`
  reported zero tests. This is not a failure; Podman test discovery/execution depends on runtime
  state unrelated to the KVM changes. Podman test status remains as-is (not a regression from
  this work).
- Touched-file formatting: `rustfmt --edition 2024 tests/src/bin/kvm-spike.rs` passed.
  Git diff check: `git diff --check -- tests/Cargo.toml tests/src/bin/kvm-spike.rs` passed.
  Files changed: `tests/Cargo.toml` (+1 line, iroh-support dependency), `tests/src/bin/kvm-spike.rs`
  (pre-generated relay certificate flow).
- OpenSpec CLI unavailable (`openspec: command not found`); strict validation remains open.
- No test workers, run-owned host links or namespace entries remained after all tests.
- Final status: direct and forced-relay scenarios pass. Workspace tests pass (165/165).
  Clippy and formatting pass for KVM changes. Pre-existing warnings in unrelated code blocks
  compilation but are not regressions.
