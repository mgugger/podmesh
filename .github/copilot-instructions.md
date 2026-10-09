# Podmesh Project Instructions

## Source of Truth

- OpenSpec is the sole active requirements, decision, and change-tracking workflow.
- Start with [OpenSpec ownership and workflow](../openspec/README.md), then the relevant source
  specs, [accepted decisions](../openspec/decisions.md), and change tasks. Do not load all artifacts
  for a small local change.
- Use the existing OpenSpec skills/prompts for proposing, exploring, implementing, and archiving.
  Follow the project-specific adaptive workflow below; do not load retired process templates.
- Do not recreate `aidlc-docs/`, legacy rule directories, separate state files, or transcript audits.
- README/deployment docs and the PoC scope document explain source specs; reconcile discrepancies
  instead of maintaining competing guarantees. Track unfinished work in one OpenSpec change.

## Adaptive Workflow

- Assess scope/risk for Podmesh's affected workload flow, ask structured clarifying questions,
  explain trade-offs, seek approval at meaningful checkpoints, implement incrementally, and review.
- On resume, read the relevant OpenSpec change tasks, design and verification; briefly summarize
  completed work, the next step and blockers. Do not reload the whole project or create session state.
- For substantial or unclear work, create or update `openspec/changes/<change>/proposal.md` for
  intent/scope, `design.md` for decisions, questions/answers and approval scope, and `tasks.md` for
  the implementation sequence. Add `specs/<capability>/spec.md` deltas when behavior changes.
  Use `openspec new change <change>` and schema instructions; reuse an existing owning change.
- Resolve material questions before implementation. Present options and a recommendation in chat,
  then record settled answers in the change's design, not a separate question-file workflow.
- Before substantial implementation, present scope, approach and verification for approval.
  An explicit instruction to implement an already presented plan counts as approval; do not ask
  again for that same scope. A vague request is not approval for an unseen substantial design.
- Small, clear fixes use a minimal existing/new OpenSpec change record and focused checks without
  mandatory staged ceremonies. Ask again if constraints, scope or security assumptions change.
- Keep `tasks.md` current after each verified increment; store commands, results, limitations and
  review findings in `openspec/changes/<change>/verification.md`. Do not mark blocked work complete.
- Finish with a review summary of delivered work, checks, remaining risks and next steps. User
  acceptance is distinct from implementation/test completion; record it only when actually given.
- Update `openspec/decisions.md` for durable accepted rationale and source specs for implemented
  requirements. Completed history stays in archived OpenSpec changes, not a parallel lifecycle tree.

## Greenfield PoC Focus

- Podmesh is greenfield: optimize for a small understandable implementation, not enterprise delivery
  stages or speculative legacy compatibility. Do not add adapters, migration frameworks or general
  service abstractions without a concrete requirement; keep existing user data and validation safe.
- Focus on a single Pod/Deployment with fixed replicas, resource-aware client placement, encrypted
  owner-agent control, identity-based authorization, stateless schedulers and direct sidecar/proxy flow.
- No workload clustering, desired-state controller, automatic relocation/scaling, persistent volumes
  or global isolation promises. Volatile proxy grants and manual recovery are accepted PoC tradeoffs.
- Keep the roles clear: podctl owns owner decisions; agents own admission/accounting/Podman;
  schedulers discover and relay; proxy/sidecar own workload traffic; protocol/crypto own validation.
- A plan should name the affected flow, trust boundary, failure cases and focused tests. Do not
  generate a catalog of generic design, infrastructure or compliance documents for each change.

## Engineering

- Preserve the scoped PoC boundaries in OpenSpec. No implicit global zero-trust, isolation,
  exactly-once, compatibility, or release-readiness claims.
- Prefer existing owning abstractions, typed validators, named bounds and contextual errors.
  Keep security decisions explicit and refactors local; do not add frameworks or split files merely
  to reduce line count. Preserve public interfaces and wire formats unless the requested fix needs change.
- Keep secrets out of diagnostics and test output. Use runtime logging and intentional CLI output.
- Preserve existing user changes. Use targeted tests immediately after edits, before widening scope.
- Keep ordinary unit, integration, adversarial and applicable property tests. Reuse existing bounded
  generators and reproducible seeds; do not replace security regression coverage with happy-path tests.
- Fuzz campaigns, soak tests and dedicated release-evidence/SBOM/bundle pipelines are removed scope.
  Do not recreate them without an explicit user request. Keep runtime metrics and normal CI checks.

## Verification

- Focused tests: `cargo test --locked -p <package> [--test <target>]`.
- Workspace: `cargo test --quiet --locked --workspace`.
- Lint: `cargo clippy --locked --workspace --all-targets -- -D warnings`.
- Specifications: `openspec validate --all --strict --no-interactive`.
- Format only touched code and avoid unrelated cleanup. Run long terminal checks alone; parallel
  commands in this editor can reuse a shell and interrupt a running check.
- Report actual checks in a short change verification note and leave unverified tasks incomplete.
  Mock/default tests, optional real Podman execution and multi-host checks are distinct; no passing
  test or checked task certifies production readiness. No separate release-gate process is required.