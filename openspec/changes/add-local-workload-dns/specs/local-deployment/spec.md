## MODIFIED Requirements

### Requirement: The VM DNS sample SHALL prove cross-workload discovery
The opt-in VM mesh SHALL deploy separate same-owner client/backend workloads on different agent
VMs. The client SHALL use its ordinary application resolver to obtain a synthetic internal VIP
and reach the backend through a distinct Service/targetPort mapping. Its local listener SHALL
not satisfy the backend request. These checks SHALL NOT imply a native VM workload backend or
separate physical hosts.

#### Scenario: Application discovers a remote backend
- **WHEN** the client resolves the backend's canonical internal name and connects to its Service port
- **THEN** it receives a synthetic service VIP and the remote backend's pod-specific response
- **AND** the client has no listener on that backend target port

#### Scenario: Cross-VM service traffic requires the forced relay
- **GIVEN** the VM test denies direct UDP paths and the DNS sample has succeeded
- **WHEN** the test temporarily blocks relay transport
- **THEN** the remote service request fails and recovers only after relay transport is restored
- **AND** normal cleanup uses acknowledged, non-forced workload deletion

### Requirement: The two-tier VM sample SHALL execute a real database query
Separately selectable VM tests SHALL deploy the actual PostgreSQL/frontend sample manifests on
distinct agents, preload the declared database image, verify synthetic workload DNS, and require
a real SQL result through frontend ingress. Normal and forced-relay modes SHALL retain acknowledged
cleanup. Demo database storage SHALL remain pod-ephemeral.

#### Scenario: Frontend queries PostgreSQL on another VM
- **WHEN** the sample starts in the opt-in VM mesh
- **THEN** the frontend resolves its configured database alias to a synthetic VIP
- **AND** ingress returns the expected PostgreSQL query result from a different agent VM

#### Scenario: A fresh query requires forced relay transport
- **GIVEN** the two-tier sample succeeds with direct inter-guest UDP denied
- **WHEN** relay transport is blocked and the frontend executes a fresh SQL query
- **THEN** that query fails and succeeds again only after relay transport is restored
- **AND** both sample pods are absent after non-forced deletion

## ADDED Requirements

### Requirement: The three-replica Podman workflow SHALL exercise internal DNS and peer traffic
Feature-gated real Podman tests SHALL retain distinct-agent placement and sequential-update ingress checks, and add workload-namespace DNS and remote-replica TCP checks. Evidence SHALL distinguish direct sidecar traffic from proxy forwarding and local application responses. Three agents on one Podman host SHALL NOT be described as three physical hosts.

#### Scenario: Service resolves across the deployed replicas
- **WHEN** the three-replica Service is deployed and bootstrap completes
- **THEN** workload DNS returns synthetic internal addresses over UDP and TCP
- **AND** connections reach authenticated remote replicas

#### Scenario: Direct path survives proxy shutdown
- **GIVEN** a verified direct Iroh path between workload peers
- **WHEN** proxy processes and their co-hosted relays are stopped
- **THEN** internal DNS and direct peer TCP continue for the bounded test
- **AND** the result makes no claim about relay-dependent NAT connectivity

### Requirement: Replica loss SHALL be tested without implicit rescheduling
The initial DNS test SHALL verify local retry and eventual exclusion of expired replicas without invoking podctl during the failure interval. It SHALL NOT claim replacement, relocation, fencing or desired-state recovery.

#### Scenario: One replica stops
- **WHEN** one of three sidecars stops serving and refreshing records
- **THEN** new service connections can use surviving replicas
- **AND** the failed replica's record eventually expires without a replacement being scheduled

### Requirement: DNS tests SHALL preserve deterministic upstream checks
Tests SHALL use deterministic non-internal DNS fixtures and verify the original runtime resolver path. Live public Internet resolution SHALL be optional and reported separately. Test tools SHALL be provisioned explicitly rather than assumed present in the scratch sidecar image.

#### Scenario: Internal lookup fails
- **WHEN** an internal service is unavailable
- **THEN** the fake external resolver observes no query for that internal name

#### Scenario: Non-internal name is queried
- **WHEN** a workload requests a controlled external or runtime name
- **THEN** forwarding succeeds through the configured upstream

### Requirement: Unattended renewal and identity lifecycle SHALL be exercised
Tests SHALL exercise short-lived child renewal without owner interaction, sidecar restart, sequential update and cleanup. They SHALL distinguish valid delegated renewal from finite-parent expiry and SHALL verify copied credentials cannot authorize another endpoint.

#### Scenario: Child lifetime elapses
- **WHEN** the CLI is inactive and delegated authority remains valid
- **THEN** renewed endpoint-bound credentials maintain authorized peer operation

#### Scenario: Sidecar restarts or is replaced
- **WHEN** a normal restart or authorized update occurs
- **THEN** persistent identity/incarnation rules prevent duplicate stale membership

#### Scenario: Real Podman execution is unavailable
- **WHEN** images, sockets or runtime prerequisites are missing
- **THEN** reporting distinguishes unexecuted runtime checks from passed unit tests or compilation
