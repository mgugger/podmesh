## Purpose

Centralize the existing MVP release acceptance and reproducibility obligations without treating
implementation checkboxes or successful local tests as a substitute for retained release evidence.

## ADDED Requirements

### Requirement: Release acceptance SHALL require all ten evidence gates

An MVP release SHALL require evidence for all ten gates below. Missing mandatory evidence and
unaccepted failures SHALL block acceptance; documentation migration SHALL NOT complete a gate.

1. Fail-closed proxy trust and explicit TOFU tests.
2. Canonical validation for every workload control message.
3. Default and real Podman end-to-end suites.
4. Required examples, adversarial cases and property tests in CI.
5. All configured security/manifest fuzz targets completing their default smoke budget.
6. Dependency advisory/license/source checks without unaccepted blocking findings.
7. Workspace and release-image SBOM evidence for the actual subjects.
8. Metrics demonstrated without secret leakage or uncontrolled cardinality.
9. Published performance baseline and partition evidence without correctness, security, deadlock,
   panic, corruption or unbounded-growth failures; optional soak handled explicitly.
10. Source OpenSpecs and operator claims using scoped zero trust and accepted limitations.

#### Scenario: A release has no confirmed baseline

- **WHEN** local lifecycle tests passed but the release-mode baseline or final bundle is absent
- **THEN** release acceptance remains incomplete
- **AND** earlier local runs remain historical evidence, not an all-gates claim

### Requirement: Extended assurance SHALL remain opt-in without hiding failure

Fuzz smoke SHALL default to 30 seconds per target. Extended fuzz and the continuous one-hour soak
SHALL be independent default-off manual opt-ins. Only soak SHALL permit explicit `not_requested`
without artifacts; absence of its provider is still missing evidence. An opted-in tool failure,
missing image, scenario or observation SHALL remain blocking. Enabled release soak SHALL run at
least 3600 seconds with observations at most 60 seconds apart. Numeric performance changes are
informational; no production SLO or mandatory extended campaign is added.

#### Scenario: Soak is not requested

- **WHEN** a release run explicitly disables soak
- **THEN** its provider reports `not_requested` without claiming a passing soak
- **AND** baseline, partition and all other mandatory evidence remain required

### Requirement: Evidence SHALL be bounded reproducible and failure-preserving

Evidence SHALL use locked dependencies, explicit schema versions, run/source/lock provenance,
bounded contained files, content digests, and reproducible property seeds or fuzz inputs.
Environment metadata SHALL record the applicable hardware, OS, topology, configuration, profile
and command without secrets. Failures SHALL NOT be replaced by automatic retries or imported
caches. Unknown versions SHALL be rejected rather than silently migrated.

#### Scenario: Evidence belongs to another revision

- **WHEN** a provider's provenance or digest disagrees with the current run
- **THEN** it is refused and cannot contribute a passing gate

### Requirement: Release tooling SHALL remain outside the runtime trust path

Release evidence SHALL be produced by short-lived repository/CI tooling, not a runtime service or
database. Runtime images SHALL exclude evidence/fuzz tools. CI SHALL use pinned external actions,
bounded execution and retention, read-only defaults and publication permissions only after all gates
pass. Policy exceptions SHALL require an exact approved scope and expiry, not waive entire gates.
Digests SHALL NOT be described as signatures, attestations or third-party provenance.

#### Scenario: An opted-in provider fails

- **WHEN** a required or opted-in provider fails
- **THEN** its diagnostic and failure outcome are retained
- **AND** release publication is blocked without adding a waiver just to make the run pass