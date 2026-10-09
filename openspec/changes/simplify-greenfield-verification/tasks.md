## 1. Remove Specialized Assurance

- [x] 1.1 Detach and delete the evidence Cargo package, scripts, fuzz targets/corpora, report harness/tests and release-only metadata; normal Cargo resolution and integration-library checks pass.
- [x] 1.2 Remove the Podman soak hook while retaining lifecycle and metrics coverage; the feature-enabled full-stack target compiles.
- [x] 1.3 Replace evidence CI with direct lint/build/test/dependency checks and explicit manual Podman execution; all four workflow policy tests pass.

## 2. Tailor the Greenfield Project Record

- [x] 2.1 Replace the release contract/commands with PoC scope and normal verification docs; documentation and runtime-image boundary tests pass.
- [x] 2.2 Tailor instructions, OpenSpec context and source requirements to Podmesh flows without generic release/migration obligations; removal safeguards and strict source/change validation pass.
- [x] 2.3 Rename the backlog to complete-poc-hardening and remove six release tasks as cancelled scope, leaving four runtime/interface tasks and the multi-host demonstration open; current links resolve.

## 3. Verify Retained Behavior

- [x] 3.1 Run locked workspace tests, warning-denied Clippy, touched-file formatting and Podman-feature compilation; 482 tests passed and actual checks/limitations are recorded in verification.md.