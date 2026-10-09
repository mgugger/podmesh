## Why

The user wants to retain AI-DLC's adaptive collaboration while eliminating its redundant document
tree. The first OpenSpec migration removed too much process behavior along with duplicate storage.
On 2026-09-09 the user approved keeping planning, clarification, approval, progress and review on
top of OpenSpec, then removing obsolete AI-DLC content.

## What Changes

- Preserve adaptive depth, structured questions/answers, approval checkpoints, incremental tasks,
  verification/review and session continuity using OpenSpec artifacts only.
- Make agent and artifact-generation instructions explicit about what files to create and when.
- Remove the 127 Markdown files in the retired AI-DLC tree after confirming migrated decisions,
  code inventory, historical result summaries and unfinished work have OpenSpec owners.
- Replace live links and preservation claims with the new policy; keep dated OpenSpec migration
  records as history rather than rewrite what happened on 2026-09-08.
- Add focused regression coverage for workflow instructions, legacy absence and valid links.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `development-workflow`: Preserve the collaborative workflow through OpenSpec artifacts and
  meaningful approval gates; no longer require the redundant legacy tree to be retained.

## Impact

Workspace agent instructions, OpenSpec context/guide/decisions, documentation tests and obsolete
Markdown content. No runtime behavior, wire format, dependency, PoC boundary or release gate changes.

## Non-Goals

No second state machine, transcript audit, mandatory ceremony for tiny fixes, archived-change
rewriting, release execution or completion of imported hardening tasks.