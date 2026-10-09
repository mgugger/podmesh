## Purpose

Keep project requirements, accepted decisions and implementation tasks in one lean, verifiable
workflow, preserving applicable test obligations without parallel planning or approval systems.

## ADDED Requirements

### Requirement: OpenSpec SHALL own active project decisions and work

The project SHALL use source OpenSpecs for current requirements, change designs for rationale, and
change tasks for implementation progress. An accepted decision index SHALL link to owning specs
without becoming a second requirements or task ledger. Operator guides and the stable release-claim
table SHALL reflect source specs rather than independently redefine guarantees.

#### Scenario: A guarantee changes

- **WHEN** a requested change alters observable behavior or a guarantee
- **THEN** its owning source spec and affected operator projections are updated together
- **AND** the change records rationale and verification without a parallel AI-SDLC plan

### Requirement: Retired workflow history SHALL NOT govern new work

The project SHALL preserve superseded AI-SDLC decisions and evidence as historical records, not
active rules, approvals, requirements or task state. Active agent configuration and executable
tests SHALL NOT require the legacy tree. Unfinished work SHALL transfer without being marked done.

#### Scenario: Work resumes after migration

- **WHEN** an agent or contributor resumes development
- **THEN** OpenSpec changes identify outstanding work
- **AND** no legacy welcome ceremony, question file, audit append or stage approval is required

### Requirement: Work tracking SHALL be proportional and verification-backed

The project SHALL keep one task owner per work item and mark completion only after the described
work and required checks finish. Clear implementation requests authorize their stated scope;
unresolved scope or security decisions SHALL be clarified before expanding it. Small fixes need
no mandatory multi-stage planning. Reported verification SHALL distinguish focused, default,
Podman, multi-host and release-evidence runs.

#### Scenario: Default tests pass but release evidence is missing

- **WHEN** default tests pass without the required release providers executing
- **THEN** the change records that result without marking the release gates complete

### Requirement: Applicable invariants SHALL retain property-based coverage

Changes to transformations and stateful logic SHALL identify applicable round-trip, invariant,
idempotence, oracle/model, and stateful properties. Tests SHALL use structured domain generators,
boundary cases, shrinking and reproducible seeds, defaulting to 256 cases. Equivalent existing
coverage SHALL be reused; exclusions or lossy transformations SHALL be explained. Properties
SHALL complement example/adversarial tests and run through normal Cargo/CI verification.

#### Scenario: A security transformation is extracted

- **WHEN** credential, envelope, manifest, trust or encryption preparation is refactored
- **THEN** generated cases verify preserved claims, round trips and rejection invariants
- **AND** a failing case retains a reproducible seed and shrunk input rather than being hidden by retry

### Requirement: Implementation boundaries SHALL stay explicit and lean

Changes SHALL preserve shared security validators, explicit ownership, finite bounds and timeouts,
and contextual diagnostics. Refactors SHALL preserve public APIs and wire formats unless the
requested behavior requires a documented change. New abstractions SHALL remove real duplication
or complexity, not satisfy an arbitrary file-size target. Secrets SHALL NOT enter logs or evidence.

#### Scenario: Similar handlers have different authorization rules

- **WHEN** common transport setup is extracted
- **THEN** operation-specific authorization remains explicit and independently tested
- **AND** no permissive fallback or generic bypass is introduced