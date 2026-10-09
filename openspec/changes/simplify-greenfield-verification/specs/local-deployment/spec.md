## REMOVED Requirements

### Requirement: Release images and evidence SHALL remain pre-production and run-scoped

**Reason**: Release evidence and SBOM/soak providers are removed from the PoC scope.
**Migration**: Keep native runtime-only image builds and optional local Podman tests.

## ADDED Requirements

### Requirement: Local images SHALL contain only runtime assets

The image build command SHALL accept a validated optional `PODMESH_IMAGE_TAG`, preserve `latest` as
the local default, refuse cross-architecture builds, print each resulting image identity, and copy
only runtime binaries, the Podman client where required, and CA certificates into final scratch
images. Local checks SHALL NOT imply multi-architecture, mixed-version, production-readiness or
numeric SLO guarantees. Ordinary CI SHALL run tests directly and keep real Podman execution an
explicit opt-in with its socket/image prerequisites.

#### Scenario: A run-scoped image is built

- **WHEN** `PODMESH_IMAGE_TAG` contains a valid run identifier
- **THEN** all four images receive that tag and their local image identities are printed

#### Scenario: A final image copy boundary is inspected

- **WHEN** the Containerfile final stages are inspected
- **THEN** only the required runtime binaries and certificates are copied into each image