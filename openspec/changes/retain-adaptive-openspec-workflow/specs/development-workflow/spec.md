## MODIFIED Requirements

### Requirement: Retired workflow history SHALL NOT govern new work

The project SHALL retain migrated decisions, dated verification summaries and unfinished work in
OpenSpec, not require a parallel legacy tree. Redundant AI-DLC templates, rules, state, question files
and transcripts SHALL be removed after that transfer. Active configuration and tests SHALL NOT load
or recreate them. Earlier migration records remain dated history, not current preservation rules.

#### Scenario: Legacy content is removed

- **WHEN** migrated decisions, verification summaries and unfinished tasks have OpenSpec owners
- **THEN** redundant AI-DLC content is removed and live links point to OpenSpec
- **AND** no migrated unfinished task becomes complete solely because its old record was deleted

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

## ADDED Requirements

### Requirement: Adaptive collaboration SHALL produce OpenSpec artifacts

The agent SHALL preserve structured clarification, explicit trade-offs, meaningful approvals,
incremental progress and review while saving work only in OpenSpec. Intent/scope SHALL go in the
change proposal; material questions, settled answers, rationale and approval scope in its design;
the implementation sequence and progress in tasks; behavior changes in spec deltas; and executed
checks, findings and limitations in verification. Durable accepted rationale SHALL be indexed in
the central decisions document. Unresolved material questions SHALL be asked before implementation,
with options and a recommendation when useful, rather than converted to unstated assumptions.

#### Scenario: Requirements are unclear

- **WHEN** an unanswered question would change scope, design, security or verification
- **THEN** the agent asks a structured clarification in chat and records the settled answer in the
  OpenSpec design before implementing that decision

#### Scenario: An increment is verified

- **WHEN** a planned increment and its required checks finish
- **THEN** its task status and verification record are updated, with failures and blockers visible
- **AND** no duplicate stage plan or transcript audit is created

### Requirement: Session continuity and review SHALL use the change record

On resumption the agent SHALL read the relevant tasks, design and verification, summarize completed
work, next steps and blockers, and continue from that state. Completion SHALL include a review
summary of delivered work, checks, remaining risks and next steps. Implementation/test completion
and user acceptance SHALL be separate; acceptance SHALL be recorded only when the user gives it.

#### Scenario: A session resumes with partially completed work

- **WHEN** development continues in a later session
- **THEN** the agent resumes from the OpenSpec change without loading retired rules or creating
  a separate session-state document

#### Scenario: Implementation is complete but not accepted

- **WHEN** implemented tasks and checks are complete but the user has not reviewed the result
- **THEN** the agent presents the review summary without claiming user acceptance or release readiness