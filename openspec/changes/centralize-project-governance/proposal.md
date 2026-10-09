## Why

AI-SDLC and OpenSpec independently tracked requirements, decisions, approvals, and work. Divergent
guarantees and stale agent configuration repeatedly recreated conflicting sources of truth. The
user approved replacing this parallel workflow with OpenSpec on 2026-09-08.

## What Changes

- Make source OpenSpecs normative and capture accepted rationale in a central decision index.
- Migrate applicable testing/release obligations and unfinished work without marking it complete.
- Move the current code inventory to OpenSpec and retain dated verification, not duplicate task lists.
- Replace active AI-SDLC Copilot instructions and stale planning configuration with OpenSpec guidance.
- Preserve old records and rules as explicitly superseded history, with no active build dependency.
- Add documentation/configuration regressions preventing reintroduction of the retired workflow.

## Capabilities

### New Capabilities

- `development-workflow`: One active specification/decision/task system, with lean verification policy.
- `release-assurance`: Consolidated existing testing and ten-gate acceptance obligations migrated from AI-SDLC.

### Modified Capabilities

None. Runtime capability requirements and wire formats do not change in this migration.

## Impact

OpenSpec artifacts, workspace Copilot customization, documentation tests, and historical document
locations. Operator instructions and the stable release-claim table remain published projections of
source specs. No runtime behavior, dependencies, release-gate results, or external configuration changes.

## Non-Goals

No deletion of decision history, automatic archive of unrelated completed changes, recreation of
every old planning artifact, mandatory stage ceremonies, runtime hardening, or release execution.