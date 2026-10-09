## MODIFIED Requirements

### Requirement: OpenSpec SHALL own active project decisions and work

The project SHALL use source OpenSpecs for current requirements, change designs for rationale, and
change tasks for implementation progress. An accepted decision index SHALL link to owning specs
without becoming a second requirements or task ledger. Operator guides and the PoC scope document
SHALL reflect source specs rather than independently redefine guarantees.

#### Scenario: A guarantee changes

- **WHEN** a requested change alters observable behavior or a guarantee
- **THEN** its owning source spec and affected operator projections are updated together
- **AND** the change records rationale and verification without a parallel AI-SDLC plan

### Requirement: Work tracking SHALL be proportional and verification-backed

The project SHALL keep one task owner per work item and mark completion only after the described
work and required checks finish. Planning depth SHALL match scope, uncertainty and risk. Before
substantial implementation, the agent SHALL present scope, approach and verification for user
approval. An explicit instruction to implement an already presented plan counts as approval for
that unchanged scope; a vague request does not approve an unseen substantial design. Small clear
fixes need no mandatory staged ceremony. New constraints, scope or security decisions SHALL be
clarified before expansion. Reported verification SHALL distinguish focused, default, Podman,
and multi-host tests; no separate release-evidence process is required.

#### Scenario: A substantial plan is awaiting approval

- **WHEN** a substantial implementation approach has not yet been presented and approved
- **THEN** the agent creates or updates its OpenSpec plan and asks for approval before implementation

#### Scenario: An already presented plan is approved

- **WHEN** the user explicitly requests implementation of the presented plan
- **THEN** the agent records the approval scope in the change design and proceeds without repeating
  the same approval request

#### Scenario: A small clear fix is requested

- **WHEN** the scope is narrow, understood and explicitly requested
- **THEN** the agent uses a minimal OpenSpec record and focused verification without mandatory stages

#### Scenario: Default tests pass but Podman was not run

- **WHEN** default tests pass without real Podman execution
- **THEN** the change records that result without claiming container or multi-host success

### Requirement: Implementation boundaries SHALL stay explicit and lean

Changes SHALL preserve shared security validators, explicit ownership, finite bounds and timeouts,
and contextual diagnostics. Refactors SHALL preserve public APIs and wire formats unless the
requested behavior requires a documented change. New abstractions SHALL remove real duplication
or complexity, not satisfy an arbitrary file-size target. Secrets SHALL NOT enter diagnostics or
test output.

#### Scenario: Similar handlers have different authorization rules

- **WHEN** common transport setup is extracted
- **THEN** operation-specific authorization remains explicit and independently tested
- **AND** no permissive fallback or generic bypass is introduced

## ADDED Requirements

### Requirement: Greenfield work SHALL prioritize the Podmesh PoC

Changes SHALL serve single-workload fixed-replica scheduling, owner-agent encrypted control,
identity-based authorization, stateless scheduler coordination or the decoupled workload traffic
plane. Planning SHALL be limited to the affected behavior and trust boundary. The agent SHALL
prefer direct coordinated changes over speculative compatibility adapters, migrations, enterprise
process templates or new frameworks. Existing protocol validation and user data SHALL remain
protected; greenfield status is not permission to bypass security or discard user work.

Verification SHALL use normal unit, integration, adversarial and applicable property tests, plus
formatting and Clippy. Optional Podman and multi-host tests SHALL be reported separately. Fuzz
campaigns, soak tests, release-evidence providers, baseline reports, SBOM/bundle pipelines and
ten-gate certification SHALL NOT be required or recreated without a new explicit user decision.
Change verification is a short record of actual commands/results, not a prescribed evidence schema.

#### Scenario: A focused feature is proposed

- **WHEN** the change affects one workload-control or traffic behavior
- **THEN** its plan names the affected owner, interfaces, failure cases and cheapest meaningful tests
- **AND** unrelated migration, production-readiness and release-process tasks are not added

#### Scenario: A security change is tested

- **WHEN** an identity, authorization or encryption boundary changes
- **THEN** focused adversarial and applicable property tests still verify its rejection rules
- **AND** removing specialized assurance machinery does not weaken runtime validation