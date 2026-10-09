# Migration Verification

The [greenfield scope reduction](../simplify-greenfield-verification/proposal.md) later removed the
release-assurance capability and tooling. This is historical context, not a current gate or a reason
to recreate those files. Already-published migration deltas must not overwrite newer source specs
when this historical change is archived.

Historical results from 2026-09-08. On 2026-09-09 the user approved removal of the redundant legacy
tree and retention of adaptive collaboration in the
[workflow revision](../retain-adaptive-openspec-workflow/design.md). Preservation statements below
describe the initial migration, not current filesystem requirements or a reason to restore old files.

Date: 2026-09-08. User authorization: "ok, centralize in openspec and remove / update existing config".

## Completed Migration

- Source specs own guarantees; accepted rationale and AI-SDLC traceability are in `openspec/decisions.md`.
- Workflow and release-assurance source specs preserve applicable PBT, fixed PoC scope and all ten
  existing release gates, including default-off extended fuzz and soak.
- Code inventory moved to `openspec/code-inventory.md`; dated completed work is not a second backlog.
- Eleven unfinished hardening/interface/release/demo tasks transferred to
  `complete-poc-hardening-and-release-evidence`, all unchecked at migration. Historical baseline,
  Podman, SBOM and refactor successes are recorded separately from current release results.
- Former Copilot instructions preserved at `aidlc-docs/legacy-copilot-instructions.md` and the
  entire former `.aidlc-rule-details/` bundle moved to `aidlc-docs/legacy-rule-details/`.
- Active Copilot instructions replaced with short OpenSpec guidance; deprecated Plan chat mode
  replaced by a read-only modern Plan agent, and the DRY prompt points to the same workflow.
- AI-SDLC root/state/requirements carry superseded notices; old audit and designs are preserved,
  not resumed or maintained. The old inventory path is a redirect for historical links.
- Documentation tests no longer import legacy requirements. They validate source specs/configuration,
  modern customization frontmatter, absence of parallel active config, and no executable legacy
  dependency. They do not freeze backlog task status or require the preserved historical tree.
- The diagram helper defaults to OpenSpec or explicit Markdown paths, handles unclosed fences
  without tuple errors, and explicitly does not claim to validate Mermaid diagram syntax.
- OpenSpec's stale artifact rule key `spec` corrected to `specs`; arbitrary 300-line file policy removed.

## Validation

- Five focused release-documentation/configuration tests passed.
- Four existing workflow policy tests passed.
- Five Python fenced-diagram helper tests passed.
- Focused Rust formatting and warning-denied Clippy passed.
- Strict OpenSpec validation: 11 items passed, zero failed.
- Changed-file whitespace and editor diagnostics passed.
- Source specs and migration deltas are published with the same requirement bodies; this completed
  change is retained for separate archiving. No unrelated completed change was archived.

The initial config test needed an `anyhow::Context` import; it passed after local repair. An optional
independent review was unavailable due to a network timeout; local artifact review and executable
checks provided the migration verification instead.

## Not Claimed

No runtime behavior, wire schema, image, release-gate result, or production configuration changed.
No Podman, multi-host, fuzz campaign, soak, baseline or release bundle was executed. The migration
does not fix failed-deploy accounting or complete any imported work item. Original historical
evidence and prior user edits remain preserved.