## REMOVED Requirements

### Requirement: Release acceptance SHALL require all ten evidence gates

**Reason**: User removed the dedicated release-evidence and certification process for the greenfield PoC.
**Migration**: Use normal tests and project-specific checks; removed gates are not marked passed.

### Requirement: Extended assurance SHALL remain opt-in without hiding failure

**Reason**: Fuzz and soak testing are removed entirely, not merely defaulted off.
**Migration**: Remove flags, workflows, targets and providers; retain ordinary security tests.

### Requirement: Evidence SHALL be bounded reproducible and failure-preserving

**Reason**: The separate evidence model, parser, provider, report and bundle surface is removed.
**Migration**: Record actual test commands/results in a short OpenSpec change note, without a report schema.

### Requirement: Release tooling SHALL remain outside the runtime trust path

**Reason**: The release package, scripts, SBOM tooling and publication workflows are removed.
**Migration**: Keep runtime-only images and direct read-only CI checks; delete this source capability.