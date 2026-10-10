# Proposal

## Why

The existing Podman stack exercises real containers but shares host networking, storage and
Podman. A small VM mesh should exercise independent Linux hosts and also provide a practical
environment for rebuilding, restarting and debugging individual Podmesh components.

## What Changes

- Add an opt-in KVM integration/debug environment with eight VMs: three schedulers, three agents
  and two proxies. S1 has no attached agents, S2 has A1/A2, and S3 has A3; each role has its own VM.
- Run scheduler, agent and proxy binaries natively in their guests. Run actual workload containers
  and injected sidecars through each agent's guest-local Podman.
- Use stable libkrun 1.19.6 from one Rust worker process per VM, minimal Alpine Linux guest
  images, direct TAP networking and private guest-local block storage. First execute a two-VM
  feasibility slice before expanding to the eight-VM topology.
- Separate cached OS images, replaceable component binaries/configuration and run-local state.
  Support component replacement/restart, VM reboot, fresh recreation, console access and cleanup.
- Verify remote placement through S1, three distinct agent-VM replicas, owner lifecycle,
  ingress through both proxies, transparent egress and scheduler restart.
- Preserve existing default and Podman tests and the current three-proxy deployment manifests.
  This proposal changes test/debug tooling, not runtime wire protocols or workload guarantees.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `local-deployment`: add a separate opt-in VM mesh and debug workflow with independent guest
  networking, state and Podman; retain the existing container deployment requirements.

## Impact

- Expected implementation surfaces: the integration-test crate, a small VM harness/example,
  focused guest image/configuration assets under `tests/`, and the deployment guide.
- Host prerequisites: Linux, usable KVM, libkrun/libkrunfw, SSH client tools and scoped permission to
  create network namespaces/bridge/TAP interfaces. Image construction also needs pinned Alpine
  package tooling and ext4 image tools. No Terraform, libvirt daemon or orchestration service.
- Guest prerequisites: Linux virtio networking/block storage, Alpine runtime/debug essentials,
  and full local Podman/OCI/network helpers on agent VMs, not merely podman-remote.
- Scope is substantial because guest Podman/kernel compatibility, Iroh routing, certificate
  distribution and cleanup need an executed vertical slice before the complete mesh.
- On 2026-10-09 the user approved the presented two-VM libkrun feasibility slice. The full
  eight-VM expansion remains behind review of that executed slice.
- This change owns the VM harness, not the separate physical multi-host demonstration in
  [complete-poc-hardening](../complete-poc-hardening/tasks.md). One KVM host does not complete it.
