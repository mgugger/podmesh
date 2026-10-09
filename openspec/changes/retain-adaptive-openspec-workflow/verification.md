# Adaptive Workflow and Cleanup Verification

Historical workflow results. The later [greenfield scope reduction](../simplify-greenfield-verification/proposal.md)
removed dedicated assurance and renamed the unfinished-work change to `complete-poc-hardening`.
The pending delta's ordinary-test scenario was aligned with that revision to prevent a later sync
from restoring obsolete provider obligations; results below describe their original run only.

## Outcome (2026-09-09)

The user approved the presented workflow mapping and removal of redundant AI-DLC content. Agent
instructions, proposal/apply skills and slash prompts, Plan handoff, OpenSpec context, source
workflow spec, guide and decision index now preserve adaptive planning, structured questions and
answers, approval checkpoints, incremental progress, verification/review and session continuity.

Saved work uses OpenSpec proposal, design, spec deltas, tasks and verification artifacts. Approval
of a previously presented plan carries forward for unchanged scope; small clear fixes need no
mandatory staged ceremony. User acceptance is separate from implementation/test completion.

Removed all 127 Markdown files in the retired AI-DLC tree, including transcripts, duplicate plans,
templates and former rule files, then removed empty directories. Those files were untracked; this
cleanup does not claim Git preserves their contents. Migrated decisions, the code inventory, dated
verification summaries and unfinished work remain in OpenSpec, not every original transcript.

Live links now resolve to OpenSpec. The 2026-09-08 migration records remain as dated history, with
explicit supersession notes for their earlier preservation policy. The new workflow source
requirements are published; no unrelated change was archived or re-synced over them.

## Executed Checks

- `cargo test --quiet --locked -p podmesh-integration-tests --test release_documentation --test workflow_policy`:
  seven documentation/instruction tests and four workflow tests passed.
- `python3 -B -m unittest discover -s tests -p test_parse_mermaid.py`: five tests passed.
- `cargo clippy --quiet --locked -p podmesh-integration-tests --test release_documentation -- -D warnings`:
  passed; the touched Rust test was formatted with rustfmt.
- `openspec validate --all --strict --no-interactive`: all 12 items passed.
- Changed-file whitespace checks passed. Current guide, decision-index and instruction links resolve.
- Source/delta comparison: all four changed/added requirement bodies match after normalizing
  Markdown whitespace. The initial exact comparison found only formatter indentation differences.
- Both `aidlc-docs` and `.aidlc-rule-details` are absent. Remaining nonhistorical mentions are
  protective instructions or negative regression checks, not dependencies or restoration rules.
- The imported hardening/release change still has eleven unchecked tasks and no completed tasks.

## Review and Limitations

Configuration tests cover explicit artifact routing, approval/continuity guidance, frontmatter,
read-only Plan tool permissions, no executable legacy dependency, absence of retired directories
and current local links. They do not simulate an agent or guarantee future conversational behavior.

No runtime code, wire format, product guarantee, deployment configuration or release gate changed.
No workspace-wide runtime suite, Podman run, multi-host test, fuzz campaign, soak or release
provider was executed for this documentation/configuration change. Existing hardening defects and
missing release evidence remain open. Implementation is complete; acceptance of this delivered
result is not inferred from approval of the plan.