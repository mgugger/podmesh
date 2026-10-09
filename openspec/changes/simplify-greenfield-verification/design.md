## Context

The removed machinery comprised a separate Cargo evidence package, nine provider/orchestration
scripts, three fuzz targets/corpora, a baseline/partition/soak report model and CLI harness, an
optional soak hook in the Podman lifecycle test, three evidence-oriented CI paths, and a versioned
claim table. These were tooling boundaries, not runtime authorization components.

## Goals / Non-Goals

Use a greenfield PoC workflow with direct tests and lean local design. Preserve actual trust,
identity, encryption, failure-recovery, replica and traffic tests. Do not weaken runtime validators,
remove metrics, execute an unrequested real-container environment or introduce new compatibility
layers. The user's explicit scope-reduction request authorizes these removals, not new features.

## Decisions

- Delete report-only tests, fuzz code, dedicated scripts/tools, release metadata and workflow jobs.
  Keep normal property tests: generated invariants are useful regression coverage and are not the
  time-budgeted fuzz campaigns being removed.
- Keep ordinary dependency policy as a direct CI check using the existing deny configuration.
  Use a small CI pin file without scanner, campaign or artifact-publication dependencies.
- Run lint/build/workspace tests directly; compile Podman-enabled tests in normal CI and expose real
  Podman execution through a default-off manual input. Keep existing image tags and runtime-only
  image boundaries because they support local debugging, not just release reports.
- Replace the claim table with `docs/poc-scope.md`, keeping the reviewed trust boundaries and known
  defects without claim IDs, schemas, gates or certification language.
- Rename the unfinished-work change to `complete-poc-hardening` and remove six release tasks as
  cancelled scope. The four runtime/interface tasks and the real multi-host demonstration stay open.
- Tailor instructions to owner decisions, agent admission/execution, stateless discovery/relay,
  proxy/sidecar traffic and shared protocol/crypto validation. Keep adaptive questions and approvals,
  but do not generate generic enterprise/migration/assurance documents for a small PoC change.
- Keep old OpenSpec changes as dated history with supersession notices where their prescriptions
  would restore deleted tooling. Do not mark the old gates passed or reapply obsolete deltas.

## Risks / Trade-offs

- Less automated long-duration/parser-campaign coverage: accepted by the user; retained focused
  adversarial and property tests still protect actual security rules.
- Deleting harness tests can accidentally delete domain coverage: preserve the real scheduler,
  relay and partial-view tests rather than report-validator tests.
- Compiler-only verification can be mistaken for Podman success: distinguish build checks from
  actual execution in final results and normal change notes.
- The diagram helper was already absent in the current worktree, so remove its stale test/CI
  consumer rather than recreate user-removed tooling.

## Verification

Run focused CI/documentation/image checks immediately after edits, then normal locked workspace
tests and warning-denied Clippy. Compile the retained Podman feature suite without launching it.
Validate OpenSpec deltas/source specs and check active references for deleted commands or packages.