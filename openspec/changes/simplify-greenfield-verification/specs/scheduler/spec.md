## REMOVED Requirements

### Requirement: Release evidence SHALL preserve scheduler statelessness and partial views

**Reason**: Dedicated release evidence is removed from the greenfield PoC scope.
**Migration**: Retain normal scheduler recovery and partial-view regression tests.

## ADDED Requirements

### Requirement: Scheduler tests SHALL cover statelessness and partial views

Scheduler changes SHALL retain focused tests for loss and reattachment, stale-route resolution and
partial reachability where affected. An unreachable scheduler SHALL remain in the bounded
reconciliation result and SHALL NOT be reported as a complete owner view.

#### Scenario: One scheduler is absent during reconciliation

- **WHEN** the reconciliation deadline expires before an admitted scheduler completes
- **THEN** that scheduler is listed as unreachable
- **AND** the result remains partial until a later successful reconciliation

#### Scenario: The open API remains a limitation

- **WHEN** scheduler behavior tests pass
- **THEN** it does not claim client authentication, owner quota, or access control for the open API