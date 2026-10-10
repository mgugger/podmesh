# Spec Delta

## ADDED Requirements

### Requirement: The opt-in VM mesh SHALL use independent guest hosts

The VM test SHALL start three schedulers, three agents and two proxies in eight separate KVM
guests. S1 SHALL have no attached agent, S2 SHALL have A1/A2, and S3 SHALL have A3. Each agent
SHALL use its own guest-local Podman service, container store and state. Existing container
manifests and default tests SHALL remain unchanged.

#### Scenario: The VM mesh becomes ready

- **WHEN** the VM test starts with the required KVM and guest artifacts
- **THEN** eight guests have independent NICs and private writable state
- **AND** the test verifies the exact 0/2/1 agent attachment distribution
- **AND** no agent uses the host Podman socket

### Requirement: The VM test SHALL verify remote resource-aware placement and lifecycle

The VM test SHALL deploy three fixed replicas through S1 onto three distinct agent VMs, verify
resource-bound placement behavior, and exercise status, logs, update, rediscovery and deletion.
Assertions SHALL correlate owner-visible replica identities with actual guest-local workloads.

#### Scenario: An unattached scheduler relays the workload lifecycle

- **WHEN** the owner deploys, inspects, updates and deletes three replicas through S1
- **THEN** A1/A2/A3 each run one replica before deletion
- **AND** all three workloads are absent from guest Podman after deletion

#### Scenario: A request exceeds an agent's available resources

- **WHEN** a controlled workload request exceeds an advertised agent resource bound
- **THEN** that agent is excluded or rejects admission
- **AND** it does not leave an untracked running workload

### Requirement: The VM test SHALL exercise both proxies and actual workload traffic

The VM test SHALL verify ingress through each proxy and transparent egress through each proxy
in a controlled configuration. It SHALL distinguish direct from forced-relay checks using
observable path evidence and negative controls. VM boot or unconstrained HTTP success SHALL
NOT alone establish workload-plane or relay success.

#### Scenario: Both proxies forward traffic

- **WHEN** ingress and separately constrained egress phases exercise P1 and P2
- **THEN** each phase receives the expected application response
- **AND** egress interception and handling by the selected proxy are verified
- **AND** a direct-bypass-negative control demonstrates the intended egress path

#### Scenario: Direct mesh connectivity is blocked

- **WHEN** the harness permits configured relay traffic but blocks relevant direct mesh UDP
- **THEN** the selected control and workload operations succeed through relays
- **AND** path diagnostics or counters and a negative control verify the constraint

### Requirement: The VM debug workflow SHALL support targeted rebuilds without OS recreation

The debug workflow SHALL allow replacement and restart of one component without rebuilding
guest OS images or restarting unaffected VMs. Component restart and VM reboot SHALL preserve
guest-local state. Fresh recreation SHALL explicitly identify destructive state loss. Sidecar
replacement SHALL update its OCI image and affected workloads, not only a guest executable.

#### Scenario: A scheduler binary is rebuilt

- **WHEN** the operator replaces and restarts S1 in a retained debug run
- **THEN** S1 uses the selected build and retains its identity
- **AND** the other seven VMs remain running
- **AND** control operations recover within the configured deadline

#### Scenario: An injected sidecar changes

- **WHEN** the operator requests replacement of a sidecar build
- **THEN** the selected agent loads the new OCI image
- **AND** affected workloads are explicitly updated or redeployed
- **AND** the workflow verifies their actual image identity

### Requirement: VM test resource ownership and prerequisites SHALL be explicit

The selected VM test SHALL fail explicitly on missing prerequisites and bound boot, readiness,
commands and shutdown. Default teardown SHALL remove only owned processes, interfaces and
run artifacts. Debug retention SHALL be explicit and provide a cleanup operation. Keys and
bootstrap secrets SHALL NOT appear in diagnostics.

The default mesh SHALL budget one vCPU per guest, 256 MiB per scheduler/proxy and 512 MiB per
agent. Before allocating run resources, the harness SHALL check available host memory and visible
cgroup-v2 ancestor headroom for the configured guest RAM plus a 1024 MiB host reserve.
This check SHALL NOT be described as a memory reservation or protection against platform shutdown.

#### Scenario: Host memory is insufficient

- **WHEN** available host memory or visible cgroup headroom is below the guest budget plus reserve
- **THEN** the run fails explicitly before creating VM processes or networking
- **AND** the error reports available memory and the required budget

#### Scenario: A VM fails during startup

- **WHEN** startup fails or exceeds its configured deadline
- **THEN** the failure identifies the guest and phase with non-secret diagnostics
- **AND** default teardown removes already-created owned resources
- **AND** unrelated host networking and processes remain unchanged

#### Scenario: The operator retains a failed run

- **WHEN** explicit debug retention is selected
- **THEN** the failure remains visible and the owned mesh is available for inspection
- **AND** the workflow provides an explicit bounded cleanup operation

### Requirement: VM results SHALL remain distinct from physical multi-host evidence

Verification SHALL report actual VM execution, test compilation and ordinary tests separately.
It SHALL distinguish virtual disk capacity from allocated/image bytes and report measured
resource usage without claiming verified minima. A single-host KVM mesh SHALL NOT be recorded
as completion of the separate physical multi-host demonstration.

#### Scenario: A VM test passes on one host

- **WHEN** the eight-VM scenario succeeds on a single KVM host
- **THEN** verification records its topology, commands and observed resources
- **AND** it makes no physical multi-host, WAN realism or production-readiness claim
