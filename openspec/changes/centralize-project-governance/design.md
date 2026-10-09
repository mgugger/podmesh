## Context

Historical migration design from 2026-09-08. The user-approved
[adaptive workflow revision](../retain-adaptive-openspec-workflow/design.md) on 2026-09-09 supersedes
the preservation of the legacy tree and restores collaborative approval behavior. The choices
below record what happened then; do not reapply them over the current source workflow spec.

See the proposal for motivation. Runtime requirements already live in six OpenSpec capabilities;
AI-SDLC still owns active instructions, a requirements copy, code-review progress and unfinished
release work. A documentation test imports that requirements copy. OpenSpec's artifact rule key
was `spec` although the schema recognizes `specs`.

## Goals / Non-Goals

Preserve accepted decisions and incomplete work with one clear owner. Keep historical evidence and
operator claim IDs available. Do not change runtime behavior, mark unexecuted release gates passed,
or archive unrelated changes. No new transcript audit or per-stage approval system.

## Decisions

- Source specs own guarantees. A central decision index links rationale and migration traceability;
  it does not duplicate full requirement prose. Operator docs remain discoverable projections.
- Consolidate testing/workflow and release acceptance obligations in two source capabilities rather
  than importing old extension packs. This preserves applicable PBT and release gates without
  retaining a second process engine.
- Move the existing code inventory mechanically to OpenSpec. Convert its completed checklist to a
  dated review snapshot; unfinished items belong to the remaining-work change only.
- Preserve AI-SDLC files in place for historical links, with a root retirement notice and banners
  on active entry points. Move former Copilot instructions and `.aidlc-rule-details` under that
  historical root so they cannot be rediscovered as active rules. Replace Copilot configuration
  with a short link to the OpenSpec workflow and verification guidance.
- Use OpenSpec proposal/design/tasks for remaining defects and release evidence. No capabilities
  change merely by transferring unfinished tasks; that record can use `skip_specs: true`.
- Documentation tests parse real OpenSpec YAML and reject legacy runtime/test dependencies.
  Generic plan/DRY customizations must point to OpenSpec instead of starting parallel plans.

## Risks / Trade-offs

- Historical "mandatory" text can mislead readers: mark the historical tree and entry points
  superseded; active configuration and tests must not load it.
- A data-loss defect must not turn into an accepted limit: keep failed-deploy accounting open.
- Moved inventories can duplicate work state: one remaining-work task list owns all unresolved items.
- Migration validation is not release verification: preserve dated earlier test results and missing
  baseline/bundle outcomes without rerunning expensive providers.

## Migration Plan

Update config and guards, create the OpenSpec records, preserve/retire active AI-SDLC configuration,
switch documentation links/tests, validate and publish the two source specs. Keep this completed
migration available for separate archiving; its spec bodies are already synchronized. Rollback is a deliberate
workflow decision using retained history, not an automatic fallback to the old rules.