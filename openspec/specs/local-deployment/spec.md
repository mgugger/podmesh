# Local Deployment Specification

## Purpose

Podmesh must be startable locally as a realistic mesh without any operator-created secrets, so that
the system's behaviour — placement, replica spread, ingress, and lifecycle — can be exercised end to
end with `podctl`.

## Requirements

### Requirement: The local manifests SHALL deploy three schedulers and three agents

`deploy/podmesh_rootless.yml` and `deploy/podmesh_rootful.yml` SHALL each bring up three
schedulers, three agents, and three proxies with non-conflicting ports and per-component state
directories.

#### Scenario: Rootless start

- **WHEN** the operator plays the rootless manifest
- **THEN** three schedulers, three agents, and three proxies start
- **AND** agents reach Podman through the rootless user socket

#### Scenario: Rootful start

- **WHEN** the operator plays the rootful manifest
- **THEN** the same topology starts and agents reach Podman through the system socket

### Requirement: The sample topology SHALL spread agents unevenly across schedulers

The manifests SHALL attach no agent to the first scheduler, two agents to the second, and one agent
to the third. Attaching every agent to every scheduler hides the case a real multi-region mesh
always has: a scheduler a client can reach that holds none of the agents it needs.

#### Scenario: A client uses the scheduler with no agents

- **GIVEN** the sample topology
- **WHEN** a client deploys, inspects, and deletes a workload through the first scheduler
- **THEN** every operation succeeds through the peer that holds the agent's attachment

#### Scenario: Placement is not limited to the reached scheduler

- **WHEN** a client asks the first scheduler to select an agent
- **THEN** it receives a signed offer from an agent attached to another scheduler

### Requirement: The local deployment SHALL require no pre-created secrets

The manifests SHALL NOT reference operator-created secrets. All relay TLS material, relay auth
tokens, and component identities SHALL be generated on first start and persisted in the shared
state volume.

#### Scenario: Cold start on an empty volume

- **WHEN** the stack is started with no existing state volume
- **THEN** it converges without the operator creating any certificate, key, or token

#### Scenario: Identities survive restart

- **GIVEN** an existing state volume
- **WHEN** the stack is restarted
- **THEN** every component keeps its previous endpoint identity

### Requirement: Start order SHALL NOT be a precondition

Components SHALL converge regardless of the order in which they start. Components that depend on
credentials another component has not yet generated SHALL restart until those credentials exist.

#### Scenario: Agents start before schedulers have written certificates

- **WHEN** an agent starts before the scheduler relay certificates exist
- **THEN** it restarts and eventually attaches without operator intervention

### Requirement: The deploy documentation SHALL describe an end-to-end trial

`deploy/README.md` SHALL document building the images, starting the mesh, pointing `podctl` at it,
deploying a workload, spreading replicas across agents, reaching ingress, and tearing the stack
down. It SHALL NOT instruct the operator to create any secret.

#### Scenario: Operator follows the README

- **WHEN** an operator follows the documented steps on a clean machine with Podman
- **THEN** they reach a running workload reachable through the proxy ingress port
- **AND** they can list, inspect logs for, and delete it with `podctl`
