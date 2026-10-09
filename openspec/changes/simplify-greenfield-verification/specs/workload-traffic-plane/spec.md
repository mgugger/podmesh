## REMOVED Requirements

### Requirement: Release evidence SHALL execute bounded traffic scenarios

**Reason**: Mandatory baseline and optional one-hour soak evidence are removed.
**Migration**: Keep ordinary traffic, authorization, metrics and relay-recovery tests.

## ADDED Requirements

### Requirement: Traffic tests SHALL cover authorization and forwarding

Traffic changes SHALL retain affected tests for tenant/grant authorization, registration, bounded
ingress/egress, stream cancellation and direct/relay recovery. Metric cardinality and secret-safe
output checks SHALL remain ordinary tests. Real Podman ingress and transparent-egress tests SHALL
remain available when explicitly enabled, without a timed sustained-traffic requirement or SLO.

#### Scenario: A traffic check did not execute

- **WHEN** Podman or a separate-host environment is unavailable
- **THEN** the result states that real execution was not verified rather than claiming success