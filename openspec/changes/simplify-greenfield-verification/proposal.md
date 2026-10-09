## Why

Podmesh is a greenfield proof of concept, not a release-certification program. On 2026-09-09 the
user requested removal of fuzz testing, soak testing and dedicated release evidence, and a workflow
tailored to this project's size and goals instead of inherited AI-DLC process obligations.

## What Changes

- Remove fuzz targets/corpora, soak and baseline/report harnesses, evidence tooling/scripts,
  release-only policy metadata, SBOM/bundle generation and specialized evidence workflows.
- Run ordinary CI checks directly. Retain unit, integration, adversarial, property and optional
  real Podman tests, including actual scheduler/relay recovery tests and runtime metrics checks.
- Retire release gates and their pending tasks as removed scope, not passed verification.
- Replace the release claim table with ordinary PoC scope documentation and tailor OpenSpec and
  agent instructions to concrete workload flows, lean design and proportional verification.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `development-workflow`: Greenfield project-specific planning and ordinary verification without
  mandatory fuzz/soak or a release-evidence process.
- `release-assurance`: Remove the dedicated acceptance/evidence capability and all of its gates.
- `agent`: Remove release-evidence-specific requirements while retaining runtime safety requirements.
- `scheduler`: Remove release-evidence-specific requirements while retaining statelessness tests.
- `podctl-cli`: Remove release-harness obligations while retaining actual CLI lifecycle coverage.
- `message-security`: Replace evidence/fuzz-provider obligations with focused security tests.
- `workload-traffic-plane`: Remove release-specific evidence obligations, not traffic authorization.
- `local-deployment`: Replace evidence-bundle instructions with normal local verification guidance.

## Impact

Cargo workspace/lockfile, integration harness and report-only tests, CI, scripts, documentation,
agent instructions, source specs and pending work. No workload wire format or runtime security
semantics change. Dedicated release commands are removed, not replaced by permissive success stubs.

## Non-Goals

No removal of ordinary regression/property tests, metrics, trusted-agent/proxy checks or runtime
bounds. No backward-compatibility framework, production SLO, new release gate, or automatic cluster
controller. Existing unrelated code and unfinished runtime fixes are preserved.