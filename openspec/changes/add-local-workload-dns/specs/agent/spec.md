## MODIFIED Requirements

### Requirement: The agent SHALL refuse prohibited privilege and host-access manifest settings

The agent SHALL evaluate original tenant manifests against a deny-by-default pod security policy before deployment, covering init, ephemeral and application containers. It SHALL refuse privileged execution, privilege escalation, added capabilities outside the injected sidecar, `runAsUser: 0`, host namespaces, shared process namespaces, host users, host ports and tenant-authored volumes of every kind.

Podmesh-generated sidecar identity storage SHALL use only an exact separately validated internal resource definition with agent-derived names and sidecar-only mounts. This exception SHALL NOT accept tenant-selected paths, volume names, mounts or runtime sockets. All other injected fields SHALL remain validated and resource-accounted.

Only the exactly named agent-injected sidecar SHALL receive NET_ADMIN; similar tenant container names SHALL NOT inherit its exemption. These checks SHALL NOT claim host confinement or tenant network isolation: shared-network application ports and the host/Podman runtime remain trusted execution boundaries.

#### Scenario: A host escape attempt is refused
- **WHEN** a tenant manifest requests host namespaces, paths, ports, extra capabilities or privileged execution
- **THEN** deployment is refused with policy violations

#### Scenario: A privileged init container is refused
- **WHEN** an init container requests privilege
- **THEN** it is refused like an application container

#### Scenario: A tenant container cannot borrow the sidecar exemption
- **WHEN** a tenant uses a similar sidecar name or internal storage name
- **THEN** it gains neither capabilities nor storage access

#### Scenario: Internal identity storage does not enable tenant volumes
- **WHEN** the agent provisions persistent sidecar identity state
- **THEN** only the injected sidecar receives that managed mount
- **AND** the original tenant volume policy remains enforced

### Requirement: The agent SHALL execute workloads through Podman with an injected sidecar

The agent SHALL decrypt target-bound configuration, inject the sidecar and deploy through Podman. Initial metadata SHALL go only to the sidecar through its environment; enrollment and refreshed credentials/seeds SHALL use a narrow replica-bound runtime-local channel. Application containers SHALL NOT receive private identity state, tenant credentials or runtime-control sockets.

#### Scenario: Sidecar receives tenant material
- **WHEN** a pod is deployed
- **THEN** sidecar configuration carries the owner trust anchor, workload/routing identity, scoped enrollment authority, proxy endpoint records, tenant relay token and relay CA certificates

#### Scenario: A workload whose credential does not verify is not started
- **WHEN** execution authority has an invalid signature, owner, workload or issuer binding
- **THEN** the agent refuses deployment
- **AND** stored authority expiry alone does not delete an existing workload during reconciliation

#### Scenario: The routing key is derived, not chosen
- **WHEN** the agent builds sidecar configuration
- **THEN** it derives the routing key from owner and workload name

#### Scenario: Sidecar generates its identity without joining the machine plane
- **WHEN** the sidecar enrolls and receives refreshed authorization
- **THEN** the exchange is scoped to that replica through runtime-local delivery
- **AND** the sidecar receives no scheduler membership or general agent API access

## ADDED Requirements

### Requirement: Connector inspection SHALL be runtime-neutral and workload-local
The runtime SHALL transport bounded typed inspection requests to the connector inside the workload boundary, without exposing general command execution or machine-plane membership. Owner inspection SHALL use encrypted lifecycle control. Returned evidence SHALL bind owner/workload identity, actual endpoint/signing keys and a fresh challenge; self-signatures SHALL NOT establish tenant authorization.

#### Scenario: A container connector is inspected
- **WHEN** its owner requests endpoint inspection
- **THEN** the runtime addresses only that workload's injected connector
- **AND** the agent and owner client verify the returned evidence

#### Scenario: A runtime does not support inspection
- **WHEN** connector transport is unavailable
- **THEN** the operation reports unsupported/unavailable rather than inventing endpoint identity

#### Scenario: An endpoint report is replayed
- **WHEN** a previously signed report does not match the fresh command challenge
- **THEN** the operation is refused before returning successful inspection

### Requirement: Agent enrollment SHALL remain replica-scoped and restart-safe
Enrollment SHALL correlate key-possession proof with the deployment's nonce, replica and incarnation, enforce delegated issuer scope, and persist required state securely. Normal restart SHALL retain the authorized identity or require explicit re-enrollment. Runtime timeouts SHALL preserve uncertainty and per-workload ownership/accounting.

#### Scenario: Remote Podman loses completion
- **WHEN** enrollment/state provisioning has an uncertain outcome
- **THEN** the agent reports recoverable pending state rather than claiming success or releasing ownership

#### Scenario: Issuance is requested for another replica
- **WHEN** a local request names a replica outside the issuer delegation
- **THEN** no credential is issued

### Requirement: Enrollment and discovery metadata SHALL be bounded and owner-visible
The agent SHALL expose current authorized sidecar endpoint/bootstrap state only through owner-authorized workload control responses. It SHALL bound local exchanges and SHALL NOT expose a new public enrollment API or give workloads access to Podman control.

#### Scenario: Owner retrieves a seed record
- **WHEN** an authenticated owner requests a workload's bootstrap state
- **THEN** its authorized endpoint information is returned through the encrypted control path

#### Scenario: Another owner requests bootstrap state
- **WHEN** the requesting owner does not own the workload
- **THEN** the existing indistinguishable lifecycle refusal applies
