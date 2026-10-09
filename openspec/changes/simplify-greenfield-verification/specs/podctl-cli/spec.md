## REMOVED Requirements

### Requirement: Release evidence SHALL use real owner workflows

**Reason**: Baseline, soak and evidence-report harnesses are removed.
**Migration**: Run existing owner-flow integration tests directly through Cargo.

## ADDED Requirements

### Requirement: CLI tests SHALL exercise owner workflows

CLI changes SHALL use the existing owner-flow tests for affected selection, apply, update,
status/logs, catalog discovery and deletion behavior. Tests SHALL distinguish real container
execution from in-process mocks and SHALL report failure directly through the normal test runner.

#### Scenario: An owner workflow fails

- **WHEN** a tested owner operation fails unexpectedly
- **THEN** its test fails with contextual diagnostics rather than producing a synthetic success