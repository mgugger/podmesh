## REMOVED Requirements

### Requirement: The workload network SHALL NOT be treated as a tenant boundary

**Reason**: Deployments now select tenant-specific networks, while complete routing/firewall isolation remains outside scope.
**Migration**: Use on-demand managed tenant networks and supply reachable mesh endpoints; test fixtures explicitly attach their required infrastructure.

## ADDED Requirements

### Requirement: Podman workloads SHALL use managed tenant-scoped networks

The Podman runtime SHALL create/reuse a managed network per authenticated owner per Podman instance
on deployment, selecting it exclusively for that owner's pods. `--workload-network-prefix` defaults
to `podmesh-tenant`; a stable owner-key digest determines the suffix. Concurrent creation SHALL be
safe and incompatible existing networks SHALL be refused. Infrastructure connectivity is operator
configuration, not automatic runtime behavior.

#### Scenario: Deployment creates a missing tenant network

- **WHEN** an authenticated owner deploys and its network does not exist
- **THEN** the runtime creates a DNS-enabled, non-internal bridge with managed/tenant labels
- **AND** it deploys the pod exclusively on that network without attaching infrastructure
- **AND** admission and capacity queries alone do not allocate networks

#### Scenario: Agents share a tenant network safely

- **WHEN** agents using one Podman instance concurrently deploy workloads for the same owner
- **THEN** they use the same managed network
- **AND** deleting one workload does not remove that network or sibling workloads

#### Scenario: Different tenants do not share the selected bridge

- **WHEN** different owners deploy to one Podman instance
- **THEN** their workloads select distinct managed networks
- **AND** naming another owner does not authorize its mesh operations without valid credentials

#### Scenario: A conflicting network is refused

- **WHEN** the derived network name exists with incompatible ownership or configuration
- **THEN** deployment fails rather than using or modifying that network

#### Scenario: Unused networks are retained

- **WHEN** the last workload is deleted
- **THEN** the network remains for explicit operator cleanup; automatic reclamation is not implemented

#### Scenario: Topology does not replace authorization or firewall policy

- **WHEN** workloads use separate tenant bridges
- **THEN** owner credentials remain required for mesh operations
- **AND** operators supply reachable proxy/relay endpoints and routing/firewall policy
- **AND** documentation does not claim prevention of host-routed or proxy-mediated cross-tenant traffic
