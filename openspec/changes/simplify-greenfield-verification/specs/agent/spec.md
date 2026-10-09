## REMOVED Requirements

### Requirement: Release evidence SHALL preserve the agent trust and recovery boundary

**Reason**: Dedicated release evidence is removed from the greenfield PoC scope.
**Migration**: Use ordinary agent tests and retain the runtime trust/recovery requirements.

## ADDED Requirements

### Requirement: Agent tests SHALL cover ownership and failure recovery

Agent changes SHALL be verified with focused ownership, accounting, lifecycle and restart tests
appropriate to the touched behavior. Real Podman execution SHALL be reported separately from mock
runtime tests or compilation; an unexecuted test SHALL NOT be reported as passing.

#### Scenario: An agent cannot be reached during reconciliation

- **WHEN** a named agent fails to answer a recovery test
- **THEN** the view remains explicitly incomplete
- **AND** restoration is verified before that scenario can pass

#### Scenario: Agent state is destroyed

- **WHEN** an agent and its durable keys are lost rather than temporarily unavailable
- **THEN** project documentation does not claim relocation, self-healing, or agent-loss recovery