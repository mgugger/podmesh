## Context

The earlier governance migration made OpenSpec authoritative and transferred durable decisions,
the code inventory and eleven unfinished tasks. It retained 127 legacy Markdown files and removed
approval behavior more broadly than the user intended. See the proposal for the approved correction.

## Goals / Non-Goals

Keep the useful AI-DLC collaboration pattern without its separate document tree or state machine.
Do not alter runtime guarantees, create a transcript log, require a heavyweight plan for every fix,
or mistake implementation approval for acceptance of an unreviewed result.

## Decisions

- Assess complexity and risk before selecting depth. Small clear fixes remain direct; substantial
  work gets a presented scope, design and verification plan before implementation. Approving a
  previously presented plan counts as that checkpoint rather than triggering a redundant gate.
- Ask structured questions in chat with options and a recommendation where helpful. Record material
  answers and approval scope in the change design; do not recreate separate question/state files.
- Save intent, rationale, implementation tasks and verification using the existing OpenSpec schema.
  A read-only Plan agent hands artifact creation to the proposal workflow instead of gaining write
  authority. Both skills and slash prompts reference the same guide.
- Resume from change tasks/design/verification and provide a concise status update. Mark tasks
  incrementally after checks. End with findings, results, open risks and next steps; record user
  acceptance only when actually given. Task completion does not prove release readiness.
- Remove the retired Markdown tree only after current decisions, dated result summaries and
  incomplete work are confirmed in OpenSpec. This deliberately deletes old transcripts and detailed
  duplicate templates, not just a link. The files are untracked, so do not claim Git retains them.
- Keep the 2026-09-08 migration artifacts as dated history and annotate their later supersession.
  Their old preservation choice is not current policy. Future archiving must not overwrite this
  workflow revision with the earlier already-published delta.

## Risks / Trade-offs

- Natural-language instructions cannot mechanically enforce approvals: test configuration and
  artifact routing, and describe that coverage honestly instead of claiming an agent simulator.
- Removing history discards original transcripts: retain the migrated decision index and dated
  verification summaries rather than promise complete archival preservation.
- Existing read-only Plan mode cannot write artifacts: make its handoff explicit, while general
  implementation/proposal agents create the OpenSpec files.
- Gate proliferation can slow small fixes: reuse prior approval for unchanged scope and avoid
  mandatory stage ceremonies for routine edits.

## Clarifications and Approval

On 2026-09-09 the user requested removal of unneeded AI-DLC content and instructions that create
OpenSpec files, then clarified that AI-DLC behavior should be kept. The proposed mapping retained
adaptive planning, structured questions/answers, approval checkpoints, incremental progress,
verification/review and continuity in OpenSpec. The user's "ok" approved that presented scope.
No additional architectural or release-scope approval is inferred.