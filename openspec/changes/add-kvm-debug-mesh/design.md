# Design

## Context

The existing [complete stack test](../../../tests/tests/complete_rootless_stack.rs) and
[transparent egress test](../../../tests/tests/podman_transparent_egress_test.rs) start the
container manifests and inspect their containers. Those manifests share component state and
a host Podman service. Their uneven scheduler attachments are useful prior art, but their
network names, shared certificate paths and local container inspection cannot be copied unchanged
to independent guests.

[Runtime image construction](../../../deploy/Containerfile) already builds native-architecture
musl binaries and scratch OCI images. Reuse that build approach without rebuilding four complete
OCI images whenever a scheduler/agent/proxy binary changes.

Accepted decisions D-01, D-08 through D-13 and D-22 remain in force. This is a trusted local
development mesh, not hostile-host isolation, automatic failover or production certification.

### Clarifications and approval

- 2026-10-09: the user selected eight separate VMs: S1 with no agents, S2 with A1/A2,
  S3 with A3, plus P1/P2. Attachment is a mesh relationship, not VM colocation.
- The user's next clarification prioritizes debugging and rebuilding/recreating components.
- On 2026-10-09, after source verification of libkrun TAP, firmware/container features and
  Rust integration, the user said "ok, go". This approves the presented two-VM libkrun slice:
  guest-local Podman/sidecar traffic, targeted binary replacement and bounded worker cleanup.
  Full eight-VM expansion follows review of the slice, not this initial approval.
- During execution, guest clocks lagged the owner by approximately one second and exposed
  proxy-grant policy's strict issue-time check. The user explicitly approved honoring the
  existing documented 60-second issue-time skew, with regression tests. Expiry remains strict;
  owner/endpoint binding, attenuation and authorization work limits are unchanged.
- After reviewing the passing two-VM workload slice on 2026-10-09, the user approved implementing
  the complete eight-VM topology and workload/relay scenarios already listed in this change's
  tasks. Scope is three schedulers, three agents attached 0/2/1, two independent proxies, three
  distinct-agent replicas, both proxy paths, targeted scheduler restart, and a bounded forced-relay
  phase. Continue using stable libkrun workers; do not expand production runtime guarantees.
- The user also requested verification and reduction of test slowness. Measure actual phase times
  before tuning; preserve scenario checks and production grace/timeout policies. Concurrent
  run-owned VM teardown and correct HTTP-fixture PID-1 signal handling are harness-local changes.

## Goals / Non-Goals

### Lean host budget (approved 2026-10-10)

After a Codespaces shutdown interrupted an eight-VM test, the user approved reducing guest
budgets before further relay investigation. No previous-boot OOM evidence is available, so memory
pressure remains a hypothesis, not an established crash cause. Use one vCPU per guest, 256 MiB
for each scheduler/proxy and 512 MiB for each agent (2816 MiB total). The mixed-role two-VM
control guest keeps 512 MiB. Check Linux MemAvailable and visible cgroup-v2 ancestor headroom
before creating resources, reserving an additional 1024 MiB for the host. This is a point-in-time
check, not a memory reservation or protection from concurrent host activity/Codespaces shutdown.
Print worker high-water RSS during teardown; these are per-process peaks, not simultaneous total
memory or guest working-set minima. Verify the two-VM slice before an eight-VM run.

**Goals**

- Exercise actual network/kernel/storage boundaries with eight independent guests.
- Keep the edit-build-restart loop cheap and preserve identities/workloads when appropriate.
- Test resource-aware three-replica placement and lifecycle through a scheduler with no agents.
- Use both proxies and run real Podman/sidecar ingress and transparent egress.
- Make readiness failures, diagnostics and resource ownership explicit.

**Non-goals**

- Writing a hypervisor, generic multi-backend VM framework, Kubernetes or libvirt integration.
- A custom minikernel/unikernel, kernel-size competition, live migration or RAM snapshots.
- Physical multi-host proof, WAN/NAT realism, automatic relocation or proxy-grant persistence.
- Replacing the existing Podman suite, adding a compulsory VM CI job, or changing runtime APIs.
- Rootless guest Podman in the initial slice; rootful Podman is confined to disposable agent VMs.

## Decisions

### 1. Use stable libkrun workers for the approved feasibility slice

| Option | Debug/rebuild fit | Initial cost / limitation |
|---|---|---|
| QEMU/KVM subprocesses driven by Rust | Serial console, SSH, QMP, GDB stub, explicit writable disk overlays; individual VM restart without rebuilding peers | Larger host VMM package, but no effect on guest-image size |
| Firecracker subprocesses driven by Rust | Good serial/API control and standard guest SSH; small VMM | Requires compatible kernel/rootfs and TAP configuration; fewer built-in interactive debugging facilities |
| libkrun via its C API from Rust | Small embedded VMM and convenient process launch | Native library/firmware linkage and VM worker lifecycle; real NIC backend/kernel/Podman support must be proved |

The initial QEMU recommendation is superseded by source verification and the user's approval.
Pin libkrun 1.19.6's stable C-shaped API, called from Rust through a small dynamically loaded
binding. The library is Rust-built but 2.0's typed Rust API is still changing; do not depend on
its development branch for this slice. One worker process per VM isolates blocking execution
and libkrun's process-exit behavior from the parent test supervisor. No QEMU/QMP or alternate
backend is implemented.

Stable libkrun also provides `krun_add_net_tap`, which disables TSI and needs neither passt nor
gvproxy. Use this direct packet backend. Firmware 5.6.2 includes namespaces, cgroups, veth/bridge,
ext4, overlayfs, conntrack and nftables NAT. Legacy Xtables is disabled, so explicitly select
Netavark's nftables backend. Podmesh uses OUTPUT DNAT, not TPROXY. These source-level findings
are not executed Podman compatibility; the slice must prove the complete path.

No second backend is planned. A failure of the libkrun vertical slice should be reported and
resolved, not hidden by adding another hypervisor.

### 2. Separate OS, program artifacts and mutable state

Build two cached, role-independent OS images:

- **Control base:** minimal Alpine rootfs, packaged virtual Linux kernel/initramfs, BusyBox,
  OpenRC, CA certificates, iproute2 and an SSH server/client-compatible file transfer subsystem.
  Scheduler and proxy guests use this same base.
- **Agent base:** the same foundation plus local Podman, an OCI runtime, conmon, the chosen
  Podman network backend and its helpers, nftables and required libraries. Include only packages
  required by the selected configuration. Agent image preflight proves `podman kube play`,
  exec, logs and deletion; the stock kernel must support the complete workload path.

For the first slice, create an ext4 raw base and private sparse/reflink copies rather than
assuming untested qcow2 backing-chain behavior. The worker configures disks before the root,
then calls libkrun's root-disk-remount API; libkrun constructs its NullFs bootstrap internally
with the injected init and mount points. No host root directory or source/state tree is exported.
Use one agent-capable base for both initial guests to reduce build complexity; two smaller
role-specific bases remain a later optimization. No guest OS rebuild is needed for binary edits.

The common bases contain no Podmesh identity, owner key, proxy grant or SSH host key. Generate
per-VM host keys/state on first boot. Inject only a run-scoped SSH authorized public key via
a per-VM configuration disk; its private key stays in the host run directory. Pin resulting
guest SSH host keys over the harness-owned console channel. No password authentication or
global SSH trust changes.

Keep three independently replaceable inputs:

1. Pinned OS/kernel/package image: rebuild only when these inputs change.
2. Podmesh binaries and local OCI sidecar/workload archives: rebuild only affected packages.
3. Run-local configuration, state and keys: retained across process restart/reboot, discarded
   only by explicit fresh recreation or owned run teardown.

Deploy native scheduler/agent/proxy binaries over SSH into the overlay, with an atomic file
replacement followed by a targeted service restart. The guest init script supervises the role
and writes a bounded log; role binaries are not PID 1. Native musl debug builds can be copied
without rebuilding the guest OS. Default cached artifacts can be stripped release binaries;
retain host symbols and allow symbol-rich builds when debugging. No compiler in guests.

Sidecar changes are different: rebuild its OCI image, load it into the selected agent Podman
store, and explicitly redeploy/update affected workloads so a running old sidecar is not mistaken
for the new binary. Preload pinned test workload images into all three agent stores; no registry
access is needed during the test run.

### 3. Do not start with a scratch minikernel

An empty scratch rootfs alone is not a bootable VM. A Linux kernel plus BusyBox can run the
control services, but agent guests still require namespaces, cgroups, network namespaces,
veth/bridge, netfilter/nftables/conntrack, overlayfs or another validated container-storage driver,
ext4, virtio and ordinary OCI userspace. A unikernel does not supply this Podman environment.

Assemble only required Alpine packages rather than installing a full distribution. Drop build
tools, documentation and package caches from final images. Keep a shell and diagnostic network
tools: their small size cost is worthwhile for the requested debug workflow.

Report compressed base size, actual allocated base/overlay bytes, binary sizes and guest RSS
separately. Sparse disk capacity is not image size. Start planning with 1 vCPU per VM,
256-512 MiB per control guest and about 1 GiB per agent, then measure boot, convergence and
workload headroom. These are starting allocations, not verified minima or acceptance promises.
Do not specify arbitrary image-size targets before the first measured slice.

If later measurements justify a custom kernel, minimize an ordinary Linux configuration from
the passing packaged kernel and repeat all guest Podman/traffic checks. This is not phase-one work.

### 4. Use a real private packet network

Use one run-owned Linux bridge and eight TAP interfaces in a run-owned network namespace,
static guest addresses and host reachability through a dedicated management path. Scope all
privileged setup/teardown to that namespace and explicitly owned interface names; no default
route changes, flushing host firewall tables, public bridge attachment or broad cleanup.

Separate per-VM MAC/IP identity and virtio NICs replace pod-local localhost/shared DNS names.
Agents' workload bridges and container stores exist only inside their own VMs. No host Podman
socket and no shared writable host state mount. Host `podctl` addresses S1 by its VM address;
the harness reaches guests for administration without giving the workload a management shortcut.

Run without runtime Internet dependencies, using locally loaded images and a bounded test HTTP
destination in the controlled network. Reject address/subnet collisions before mutation.
The management and test network are trusted fixtures, not promised tenant isolation.

Initial networking is directly routed on one bridge. Prove relay behavior in a separate bounded
phase by blocking direct mesh UDP while permitting the configured HTTPS relay paths. Distinguish
machine-plane and workload-plane rules, exclude management traffic, and remove only owned rules.
Success must be linked to the forced path by counters/available transport diagnostics and a
negative control; an unconstrained successful HTTP request does not prove relay use.

### 5. Bootstrap without shared private state

Use the existing peer URL, endpoint-record, agent scheduler URL and relay configuration surfaces.
All URLs, advertised addresses and relay certificate names must be reachable/match guest addresses.
Do not reuse container-only names or `localhost` peer URLs.

Schedulers already generate relay credentials, but the manifests distribute public certificates
through a shared state volume. Replace that fixture convenience with a two-stage harness bootstrap:
start each scheduler with its local relay configuration to generate its own public certificate,
copy only public certificates to each consumer using authenticated management access, then
restart with the complete relay map/peer URLs. Prove this startup ordering in the vertical slice.
Private signing/TLS keys remain guest-local. Do not invent a production certificate-distribution
protocol or change runtime validation just to support the test.

P1 alone publishes the existing workload relay bootstrap fixture; P2 adopts its relay mesh secret
using the existing trusted-network bootstrap flow. Both proxies issue their normal identity
records. Bootstrap secrets must not enter diagnostics. Host podctl uses a fresh owner key directory,
pins the three fixture agent signing identities and trusts both fixture proxies through the
existing certificate workflow; do not default to `trust_any_agent`.

Assert A1/A2 attach only to S2, A3 only to S3, and none to S1 before exercising remote placement.
Do not hide a remote-forwarding regression by attaching all agents to all schedulers.

### 6. Share lifecycle code between test and interactive use

Keep a small feature-gated VM support module in the existing integration-test crate.
Use it from a focused `kvm-tests` integration target and a small example/debug entrypoint.
No standalone daemon or generalized orchestration package.

Proposed operations, names to be finalized during implementation:

- Start eight VMs and wait for kernel, SSH, role health and exact attachment readiness.
- Show addresses, component build identity, console/log locations and relevant state paths.
- Run guest shell/command; rebuild/copy/restart a selected role; reboot a selected VM.
- Explicitly recreate a selected VM from its clean base; warn that identities/workloads are lost.
- Explicitly redeploy a changed sidecar image into selected workloads.
- Stop the owned run and remove only its resources.

For interactive debugging, the Rust harness remains a foreground supervisor while the user
operates on the guests from another terminal. This avoids inventing detached daemon lifecycle.
Tests clean up on success, failure and handled interruption, with bounded graceful shutdown and
specific owned-PID termination as a fallback. Rust Drop is best-effort, not a guarantee against
SIGKILL/host failure; record run ownership so an explicit cleanup command can remove stale resources.

Failure retention is an explicit mode: keep the supervised mesh and private run directory for
inspection, indicate the test failure and show cleanup instructions. The default leaves no VMs
or host network resources behind. Do not print raw state files, credentials or unfiltered secret
configuration as diagnostics.

### 7. Assert the distributed flow, not just VM boot

Reuse public podctl/protocol helpers and existing manifests where applicable; replace Podman
container-stack inspection with guest-local SSH commands. Do not refactor the entire current
suite to introduce a generic container/VM adapter.

The focused end-to-end scenario checks:

1. Eight independent boots; exact 0/2/1 attachments; separate agent Podman services and stores.
2. Signed resource offers observed through S1; three fixed replicas placed on distinct A1/A2/A3
   VMs, verified against owner results and each guest's actual running workload containers.
3. A controlled resource request above an advertised agent bound is excluded/rejected without
   orphan runtime state. Configure deterministic agent capacities with sufficient guest headroom.
4. Status, bounded logs, fixed-placement update and catalog rediscovery through S1 and another
   scheduler entrypoint. Each response has the expected replica count and identities.
5. Ingress through P1 and P2 independently, with the expected application response; exercise each
   proxy rather than accepting success through either.
6. Transparent egress via a controlled destination. Isolate a one-proxy grant/configuration phase
   for each P1/P2 so both egress paths are proved. Assert nftables interception, proxy handling
   and response; include a direct-bypass-negative control.
7. Restart S1, the client's unattached entrypoint; assert bounded control recovery. Separately
   stop scheduler/agent processes after deployment and check the already established workload
   traffic continues, without claiming reconnect or new-placement guarantees while they are down.
8. Restore control processes; delete all replicas through S1 and verify guest pods and associated
   runtime state are removed. Finish by stopping VMs and owned host networking.

Build the direct-connectivity scenario first. Then exercise the same critical control and traffic
operations with forced relay constraints, without multiplying into a general network-chaos matrix.
Existing negative identity/tamper suites remain unchanged; this change does not recreate every
protocol regression inside eight VMs.

## Risks / Trade-offs

- Guest kernel/package compatibility is the largest early unknown. First prove an agent can start
  real Podman workloads with sidecar nftables; do not defer this until eight-VM orchestration exists.
- Guest container addresses may not be routable on the outer bridge. Configure supported advertised
  endpoints/relay paths and test the actual connection mode; never infer direct sidecar connectivity.
- Replacing an agent binary requires restarting that agent and its reconciliation with existing
  Podman state. Fresh recreation destroys it. Neither operation implies desired-state recovery.
- Proxy restart loses in-memory grants; explicitly repost normal grants during debug recovery.
- SSH/debug tools increase guest size slightly but reduce rebuild/debug complexity substantially.
- A one-host VM mesh still shares physical CPU, disk and failure domains. Do not mark the physical
  multi-host task complete or claim WAN/region/network-isolation proof.
- Repeated guest runs need bounded storage/log retention and measured host capacity. No numeric
  minimum RAM/image-size guarantee is accepted before measurement.

## Verification and sequencing

1. Prove one agent guest plus one control guest: boot, SSH replacement, local Podman, sidecar
   nftables, packet networking, certificates and cleanup. Report sizes and time measurements.
2. Expand to the exact eight-VM mesh; verify placement/lifecycle and each proxy independently.
3. Add targeted restart/debug operations and a bounded forced-relay run.
4. Preserve default/PODMAN behavior, add focused unit checks for topology/configuration/ownership,
   and document commands, observed allocations and limitations.

Planned checks include:

- `cargo test --locked -p podmesh-integration-tests` for default harness/configuration regressions.
- `cargo test --locked -p podmesh-integration-tests --features kvm-tests --test kvm_mesh -- --nocapture`
  for actual VM execution; missing KVM/tooling fails explicitly when selected.
- A touched-code formatting check and targeted Clippy with `kvm-tests` enabled.
- `openspec validate --all --strict --no-interactive`.

These commands are planned, not executed results. Record actual VM execution separately from
compilation and normal tests in [verification](verification.md).

## Upstream references

- [QEMU disk images and qcow2 backing files](https://www.qemu.org/docs/master/system/images.html).
- [QEMU GDB debugging](https://www.qemu.org/docs/master/system/gdb.html).
- [libkrun networking, firmware and device support](https://github.com/containers/libkrun#networking).
- [Firecracker guest kernel/rootfs/TAP prerequisites](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md).

Pin actual guest, kernel and VMM versions during the executed feasibility slice; these documentation
references alone do not establish compatibility or a tested minimal size.
