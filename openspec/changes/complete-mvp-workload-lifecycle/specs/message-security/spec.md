## ADDED Requirements

### Requirement: Workload updates SHALL be owner-signed and agent-encrypted

An update SHALL be signed by the namespace owner, encrypted to the currently selected agent, and
bound to the workload id, target agent signing key, expected revision, requested revision, response
KEM key, issue time, expiry, and nonce.

#### Scenario: Scheduler changes the requested revision

- **WHEN** a scheduler modifies any update field or ciphertext byte
- **THEN** the selected agent refuses the update

#### Scenario: Update is redirected to another agent

- **WHEN** a valid update is relayed to an agent other than its signed target
- **THEN** the receiving agent refuses it

#### Scenario: Update is issued by another owner

- **WHEN** a key other than the workload owner signs an update
- **THEN** the agent refuses it without revealing whether the workload exists

### Requirement: Update responses SHALL prove the resulting revision

The agent's encrypted, signed update response SHALL bind the request id, workload id, previous
revision, resulting revision, agent signing key, runtime identifier, and completion time.

#### Scenario: Stale response is substituted

- **WHEN** a response from another update or workload is returned to `podctl`
- **THEN** response binding validation fails
- **AND** the local catalog is not advanced

