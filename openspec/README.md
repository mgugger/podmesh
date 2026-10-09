# OpenSpec Project Record

OpenSpec is the sole active source for Podmesh requirements, accepted design decisions, and change
tracking. This policy was approved on 2026-09-08. AI-SDLC is retired, not a second approval workflow.

On 2026-09-09 the user retained AI-DLC's adaptive collaboration on top of OpenSpec: planning,
structured clarification, approval checkpoints, progress, verification/review and session continuity.
Only the duplicate storage and legacy process engine are retired.

Podmesh is a greenfield project. Plans should name the affected workload flow, trust boundary,
failure cases and meaningful tests, not create generic enterprise delivery or migration artifacts.
On 2026-09-09 the user removed fuzzing, soak and the dedicated release-evidence process. Ordinary
security/property tests and optional real-container/multi-host checks remain; old assurance
decisions and change snapshots are historical, not instructions to restore that machinery.

## Document Ownership

| Information | Authoritative location |
|---|---|
| Current behavior and guarantees | [Source capability specs](specs/) |
| Accepted rationale and PoC boundaries | [Decision index](decisions.md), with links to owning specs |
| Proposed work and implementation progress | `changes/<change>/{proposal,design,tasks}.md` |
| Change-specific tests and results | Supporting evidence/verification in that change |
| Completed design history | `changes/archive/` |
| Code map and dated review snapshot | [Code inventory](code-inventory.md); not another backlog |
| Agent context and artifact conventions | [config.yaml](config.yaml) |

The [README](../README.md), [deployment guide](../deploy/README.md) and
[PoC scope](../docs/poc-scope.md) explain source specs, not independent decision authorities.
Podmesh is greenfield: record normal test commands and limitations in the relevant change. There
is no dedicated release-evidence or certification process; older migration records describe retired scope.

## Working on a Change

1. Read the relevant source specs and decisions; use `openspec list --json` to find current work.
2. Assess scope, uncertainty and risk; choose minimal or substantial planning accordingly. For saved
   work, reuse an owning change or run `openspec new change <name>`, then follow schema instructions.
3. For substantial work, create the proposal/design/spec deltas/tasks and present the scope,
   trade-offs, implementation sequence and checks for approval before implementation. Ask material
   questions in chat with options/recommendations and record settled answers in the design. Approval
   to implement an already presented plan counts; do not ask again for unchanged scope.
4. Small clear fixes use a minimal change record and focused checks without mandatory stages. Ask
   again if scope, constraints or security assumptions change. Do not create parallel plans or audits.
5. Implement in verified increments, update tasks as they finish and record actual commands/results
   and blockers in the change's verification. On resume, read these artifacts and summarize the
   current position instead of restarting the process.
6. Present a review summary with delivered work, checks, open risks and next steps. Do not conflate
   task completion with user acceptance or release readiness. Record acceptance only when given.
7. Keep source specs and operator projections aligned; run
   `openspec validate --all --strict --no-interactive`. Archive a completed change when finalizing
   it, syncing its deltas once. Do not archive unrelated
   changes just because another migration is complete.

## Artifact Mapping

Create these files under `openspec/changes/<change>/` as required by the change's scope/schema:

| Activity | Artifact |
|---|---|
| Intent, problem and scope | `proposal.md` |
| Questions, answers, alternatives, rationale and approval scope | `design.md` |
| Observable behavior changes | `specs/<capability>/spec.md` deltas |
| Implementation plan and incremental progress | `tasks.md` |
| Executed checks, results, review and limitations | `verification.md` |

Durable accepted rationale is indexed in [decisions.md](decisions.md); implemented guarantees live
in source specs. Read-only Plan mode hands saved artifact creation to the OpenSpec proposal agent.

## Current Work

- [Remaining PoC hardening](changes/complete-poc-hardening/tasks.md):
   runtime defects, interface follow-ups and the multi-host demonstration remain open.
- `complete-mvp-workload-lifecycle` is recorded as implemented; its existing tasks/history are
   preserved. Completion of that change does not imply all environments were tested.
- [Governance migration](changes/centralize-project-governance/tasks.md): AI-SDLC retirement and
   configuration validation; new source specs are already published. Archive separately when desired.
- [Adaptive workflow](changes/retain-adaptive-openspec-workflow/tasks.md): restored collaborative
   behavior using OpenSpec artifacts and removal of the redundant legacy tree.
- [Greenfield simplification](changes/simplify-greenfield-verification/proposal.md): removal of
   dedicated assurance tooling and project-specific workflow rules.

Each work item has one owning change task list. The inventory is a dated review snapshot; accepted
decisions and historical verification do not serve as duplicate completion trackers.

## Retired Content

The redundant AI-DLC document/rule tree was removed by user decision on 2026-09-09. Migrated
decisions, dated verification summaries and unfinished work remain in OpenSpec; original duplicate
templates and transcripts are not retained here. Do not recreate that tree or use old instructions
to govern new work. The 2026-09-08 governance migration records its earlier preservation choice as
history; this workflow revision supersedes that choice.