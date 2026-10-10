# Tasks

The user approved the two-VM libkrun feasibility slice and, after review, the full eight-VM
expansion on 2026-10-09. The direct mesh is executed; forced-relay and interactive/debug
requirements remain incomplete. Per-phase measurement and speed fixes were requested separately.

## 0. Verified two-VM feasibility slice

- [x] 0.1 Select and execute the approved stable libkrun approach on the available x86_64 KVM host.
- [x] 0.2 Add a feature-gated Rust worker/supervisor, run-owned guest network, private disks,
  per-run SSH identity and pinned guest host keys.
- [x] 0.3 Build and boot an Alpine ext4 guest; verify firmware kernel, guest Podman cgroup v2,
  overlay storage, namespaces, nftables, and scheduler/agent guest connectivity.
- [x] 0.4 Run a guest-local Podman workload through scheduler/proxy/agent, verify ingress and
  transparent egress, replace/restart the scheduler while preserving identity/workload, then
  verify logs, deletion and default cleanup.
- [x] 0.5 Honor the approved proxy-grant issue-time skew without extending expiry, and update
  rustables to the release that fixes owned netlink descriptor double-close.

## 1. Feasibility slice and guest images

- [x] 1.1 Confirm implementation approval, host architecture and scoped network permissions;
  select pinned stable libkrun/firmware/Alpine inputs without expanding runtime guarantees.
- [ ] 1.2 Build cached minimal control and agent bases with direct-boot kernel/initramfs,
  SSH/console, per-VM key generation and private sparse disks; omit compilers and package caches.
- [x] 1.3 Boot an agent guest and a control guest on an owned virtio packet network; prove guest
  cgroups/namespaces, Podman kube play/exec/logs/delete, injected sidecar nftables and reachable
  control/relay addresses. Fail explicitly on unsupported host or guest prerequisites.
- [ ] 1.4 Demonstrate one-component binary replacement/restart without OS rebuild, stable guest
  state and cleanup after partial startup; record actual base/overlay sizes, RAM and timing.
- [x] 1.5 Honor the approved documented proxy-grant issue-time skew, preserving strict expiry
  and owner/endpoint binding; verify boundary regressions and actual guest acceptance.

## 2. Lean Rust orchestration

- [ ] 2.1 Add feature-gated VM lifecycle support to the integration-test crate and one shared
  interactive entrypoint; use existing types/helpers instead of a multi-backend framework.
- [x] 2.2 Implement run-owned namespace/bridge/TAP setup, collision checks, static addressing,
  management connectivity, KVM-only workers, console and bounded readiness/shutdown.
- [x] 2.3 Implement safe per-VM configuration injection, pinned SSH management, guest-local
  keys/state, public relay-certificate distribution, normal proxy bootstrap and owner trust.
- [ ] 2.4 Add the exact eight-VM configuration and assert 0/2/1 attachment and independent agent
  Podman stores; preload pinned sidecar/workload OCI images into each agent guest.
- [ ] 2.5 Verify cleanup on ordinary success, failure and handled interruption, and explicit stale
  run cleanup; test owned-path/process/interface boundaries without broad host mutation.

## 3. Distributed workload scenario

- [x] 3.1 Through S1, verify signed capacity offers, deterministic resource rejection/exclusion
  and three replicas on distinct A1/A2/A3 VMs, matching actual guest-local containers.
- [x] 3.2 Verify status/log/update/rediscovery through S1 and another scheduler entrypoint;
  confirm fixed-placement identities and exact replica response counts.
- [x] 3.3 Verify ingress independently through P1/P2 and transparent egress through each in
  controlled phases, with nftables/path evidence and a direct-bypass-negative control.
- [x] 3.4 Restart S1 without identity loss, verify bounded control recovery, and separately
  demonstrate existing workload traffic while scheduler/agent processes are stopped.
- [x] 3.5 Restore control, delete through S1, verify guest workload/state cleanup, and teardown
  all owned VMs and networking.
- [x] 3.6 Add a bounded forced-relay pass for critical control/workload operations; assert direct
  path denial, successful relayed traffic and a negative control with only run-owned rules.

## 4. Interactive rebuild/debug workflow

- [ ] 4.1 Expose guest shell/commands, console/log paths and addresses; implement atomic
  component copy/restart, individual VM reboot and explicit destructive fresh recreation.
- [ ] 4.2 Expose selected sidecar OCI rebuild/load and workload update/redeploy, verifying the
  running image identity; document manual proxy-grant reposting after proxy restart.
- [ ] 4.3 Verify default cleanup and explicit supervised retention on failure; retain non-secret
  build/path diagnostics without dumping credentials or modifying global SSH trust.

## 5. Checks and documentation

- [x] 5.1 Add focused topology/configuration/resource-ownership checks that run without KVM;
  run targeted default tests, touched-file formatting and Clippy with the VM feature enabled.
- [x] 5.2 Execute the opt-in KVM target on a capable host; record direct/relay results separately
  from test compilation and existing Podman tests, including observed image/RAM/debug timings.
- [ ] 5.3 Update deployment documentation with prerequisites, OS versus binary rebuild commands,
  targeted restart/recreate semantics, privilege scope, retention/cleanup and known limitations.
- [ ] 5.4 Run strict OpenSpec validation, review the delivered flow and reconcile implemented
  source requirements; do not complete the separate physical multi-host demonstration task.
- [x] 5.5 Measure eight-VM phase times, fix avoidable shutdown/fixture delays without shortening
  production timeouts or weakening workload checks, and re-execute the full direct scenario.
- [x] 5.6 Apply the approved lean guest budgets, memory preflight and worker peak-RSS diagnostics;
  validate budget bounds and execute the two-VM slice before retrying the direct eight-VM mesh.
