## REMOVED Requirements

### Requirement: Release evidence SHALL exercise cryptographic parser boundaries

**Reason**: Fuzz campaigns and release provider gates are removed, not passed.
**Migration**: Retain production validators and ordinary adversarial/property coverage.

## ADDED Requirements

### Requirement: Security tests SHALL exercise cryptographic parser boundaries

Changes to credentials, trust, envelopes and encryption SHALL use focused regression and applicable
property tests for malformed/oversized inputs, tampering, wrong owner/recipient, freshness and
retained-nonce replay rejection. Tests SHALL use the production validators and preserve reproducible
failing cases. No time-budgeted parser campaign or separate report provider is required.

#### Scenario: A malformed envelope reaches the validator

- **WHEN** an envelope has invalid addressing, signature, type or size
- **THEN** an ordinary test verifies rejection before the protected operation

#### Scenario: Compatibility is not inferred from parser acceptance

- **WHEN** a wire schema version is unsupported
- **THEN** it is refused and regenerated or upgraded in coordination
- **AND** no backward-compatibility or mixed-version guarantee is implied