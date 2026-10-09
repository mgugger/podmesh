# Code Inventory and Fixes

This is the code map and dated 2026-09-08 review snapshot, migrated from AI-SDLC. It is not an
active task tracker. Updated for the 2026-09-09 greenfield scope reduction; removed tooling is no
longer part of the current code map. Unresolved items have one owner in the
[remaining-work change](changes/complete-poc-hardening/tasks.md).

## Scope

User approved expanding the lean-interface review and implementing fixes on 2026-09-08.
The inventory covers first-party code and executable configuration; generated output (`target`),
downloaded tools (`.tools`), Git internals, historical specifications, and agent instruction assets
are excluded. Inventory coverage is not a claim of exhaustive line-by-line security review.
Names below are relative to each stated directory. Tests embedded in source files belong to that
module. Existing user modifications must be preserved.

## Application Modules

| Root | Files | Responsibility |
|---|---|---|
| podctl/src | lib.rs, main.rs, bootstrap.rs, catalog.rs, cert.rs, convert.rs, trust.rs | CLI orchestration, encrypted lifecycle, bounded bootstrap, local catalogs and trust |
| podmesh-agent/src | lib.rs, main.rs, config.rs, runtime.rs, service.rs, sidecar.rs, store.rs | Assembly, admission/accounting, runtime transactions, injection, encrypted persistence |
| podmesh-agent/src/machine | mod.rs, address.rs, attachment.rs, bootstrap.rs, config.rs, control.rs, identity.rs, runtime.rs, seen.rs | Agent discovery, authenticated attachment and control dispatch |
| podmesh-agent/src/machine/config | config_tests.rs | Configuration regression tests |
| podmesh-proxy/src | lib.rs, main.rs, config.rs, egress_target.rs, ingress.rs, iroh_runtime.rs, proxy_grants.rs, relay.rs, relay_bootstrap_api.rs, restapi.rs, routes.rs, tenant_sessions.rs, workload.rs | Traffic listeners, routing, grants, relay and tenant session ownership |
| podmesh-proxy/src/iroh_runtime | egress.rs, handlers.rs, tenant_gate.rs | Typed control dispatch, authorization and raw egress |
| podmesh-scheduler/src | lib.rs, main.rs, clientapi.rs | Scheduler assembly and HTTP selection/control |
| podmesh-scheduler/src/machine | mod.rs, attachments.rs, config.rs, control_relay.rs, coordinator.rs, discovery.rs, forward.rs, gossip.rs, gossip_messages.rs, gossip_publisher.rs, identity.rs, location.rs, member_issuers.rs, members.rs, offer_handler.rs, peer_pins.rs, placement.rs, query.rs, reconciliation.rs, reconciliation_handler.rs, reconciliation_responder.rs, reconciliation_service.rs, reconciliation_tests.rs, relay_handler.rs | Stateless discovery, membership, bounded selection, one-hop forwarding and owner catalog discovery |
| podmesh-scheduler/src/machine/placement | placement_tests.rs | Placement tests |
| podmesh-scheduler/src/machine/query | query_tests.rs | Capacity-query tests |
| podmesh-scheduler/src/relay | mod.rs, access.rs, config.rs, tls.rs | Machine-relay admission and transport configuration |
| podmesh-sidecar/src | lib.rs, main.rs, egress_nft.rs, egress_proxy.rs, http_connect_proxy.rs, identity.rs, manifest_routes.rs | Assembly, tenant metadata, manifest routes and local traffic interception |
| podmesh-sidecar/src/iroh_runtime | mod.rs, connection.rs, streams.rs | Proxy connections, registration, ingress and egress streams |

## Shared Libraries

| Root | Files | Responsibility |
|---|---|---|
| shared/crypto/src | lib.rs, domain.rs, logging.rs | Key storage, domain-separated signatures, encryption, redaction |
| shared/axum_support/src | lib.rs, rate_limiter.rs | HTTP listener and per-peer request bounds |
| shared/iroh_support/src | lib.rs, node_identity.rs, raw_relay.rs, relay_credentials.rs | Endpoint identity, relay credentials, bounded raw duplex forwarding |
| shared/protocol/src | lib.rs, agent.rs, agent_control.rs, agent_control_relay.rs, biscuit_keys.rs, capacity.rs, egress.rs, endpoint_record.rs, http_proxy.rs, machine_relay.rs, manifest_policy.rs, manifest_resources.rs, manifest_yaml.rs, placement.rs, podmesh_annotations.rs, proxy_endpoint_discovery.rs, proxy_grant.rs, reconciliation.rs, reconciliation_tests.rs, relay_token.rs, replay_registry.rs, scheduler_gossip.rs, scheduler_mesh.rs, sidecar_metadata.rs, sidecar_registration.rs, workload_body.rs, workload_control.rs, workload_credential.rs, workload_stream.rs | Wire types, validation, credentials, canonical payloads and manifest/resource policies |
| shared/protocol/src/machine | mod.rs, envelope.rs, messages.rs, sidecar.rs | Canonical envelopes and handshake payloads |
| shared/protocol/src/workload_credential | credential_tests.rs | Credential regression tests |
| shared/metrics/src | lib.rs, catalog.rs, config.rs, diagnostics.rs, listener.rs, properties.rs, recorder.rs, registry.rs, snapshot.rs, startup.rs, timer.rs | Bounded metrics vocabulary, recording, rendering, lifecycle and tests |
| shared/metrics/src/encoding | mod.rs, families.rs | OpenMetrics encoding |

## Developer Tooling

| Root | Files | Responsibility |
|---|---|---|
| deploy | build_containers.sh, Containerfile, podmesh_rootless.yml, podmesh_rootful.yml, demo_deployment.yml | Images and local deployment configuration |
| workspace | Cargo.toml, Cargo.lock, deny.toml | Build and ordinary dependency policy |
| .github/workflows | ci.yml | Lint/tests/dependency checks and optional Podman execution |
| .github | ci-tool-versions.env | Ordinary CI tool/action pins |
| crate roots | Cargo.toml for each application, shared library and integration tests | Dependency and feature boundaries |
| .cargo, .devcontainer | .cargo/config.toml; .devcontainer/Dockerfile, devcontainer.json, setup-podman-storage.sh | Local build/development environment |

## Test Modules

| Root | Files |
|---|---|
| podmesh-agent/tests | iroh_capacity.rs, iroh_capacity/control_lifecycle.rs, iroh_failover.rs, scheduler_relay.rs |
| podmesh-proxy/tests | mesh_bootstrap.rs, rest_rate_limit.rs, workload_relay.rs |
| podmesh-scheduler/tests | client_rate_limit.rs, relay_server.rs, scheduler_capacity.rs, scheduler_control_relay.rs, scheduler_gossip.rs, scheduler_mesh_growth.rs, common/mod.rs, common/harness.rs |
| shared/protocol/tests | admission_refusal.rs, capacity_records_test.rs, machine_relay_grant_test.rs, proxy_grant_test.rs, workload_control_properties.rs |
| tests/src | lib.rs, mesh.rs, scheduler_node.rs, support.rs, property/mod.rs, property/proxy_trust.rs, property/proxy_trust_model.rs |
| tests/tests | complete_rootless_stack.rs, deployment_manifests.rs, metrics_observability.rs, podctl_capacity_retry.rs, podctl_deployment_flow.rs, podctl_update_flow.rs, podman_transparent_egress_test.rs, policy_validation_test.rs, process_based_stack.rs, project_documentation.rs, proxy_trust_flow.rs, proxy_trust_properties.rs, sidecar_integration.rs, workflow_policy.rs, workload_control_properties.rs |
| tests/sample_manifests | authorized_sidecar_net_admin.yml, demo_deployment.yml, demo_deployment_without_sidecar.yml, deployment_no_resources.yml, host_escape.yml, nginx.yml, nginx_with_replicas.yml, privileged_container.yml, transparent_egress_test.yml, unauthorized_net_admin.yml |
| podmesh-scheduler/tests/sample_manifests | demo_deployment.yml, nginx.yml, nginx_with_replicas.yml |
| podmesh-sidecar/tests/sample_manifests | nginx.yml |

## Completed Review Snapshot (2026-09-08)

- [x] Inventory first-party code surfaces and executable configuration.
- [x] Review CLI create/update and proxy dispatch locally; baseline CLI unit tests: 28 passed.
- [x] F-01: Deduplicate CLI capsule preparation and clarify private signing/KEM contexts; 28 existing unit tests, 10 deployment/update integration tests, and 256 generated capsule cases passed.
- [x] F-02: Centralize proxy envelope context; repair dispatch metrics finalization. Proxy unit tests (26), workload-control properties (8), and a new real-Iroh invalid/unauthorized/replayed metrics regression passed.
- [x] F-03: Remove duplicated admission-reason literals without changing the wire format or retry policy. Both capacity-retry tests and three focused agent admission tests passed; explicit wire-value regression coverage added.
- [x] F-04: Remove a single-use floating-point helper trait; all 46 metrics unit/property tests passed, plus the admission wire-value contract test.
- [x] F-05: Make raw-relay EOF shutdown cancellable; all seven raw-relay tests passed, including a permanently pending writer-shutdown regression.
- [x] F-06: Reuse the scheduler's existing clock helper instead of six identical private implementations; all 49 scheduler unit tests passed.
- [x] F-07: Preserve prefetched HTTP body and early CONNECT bytes across sidecar buffer-to-socket handoff; local TCP regression passed for both modes.
- [x] F-08: Replace manual absolute-URL parsing with the existing reqwest URL parser; HTTP defaults to port 80 and query/IPv6 request targets are preserved. All three focused HTTP proxy tests passed.
- [x] Scan remaining agent, scheduler, sidecar, shared libraries and test interfaces; record targeted detailed reads and deliberate non-changes below.
- [x] Implement the selected local defects with focused regression tests, keeping larger changes separate.
- [x] Run applicable workspace validation and record exact outcomes and remaining limitations.

## Final Verification (2026-09-08)

- `cargo test --quiet --locked --workspace`: 533 passed, zero failed, zero ignored across 71
	test targets, including doctest targets. This is the default feature set, not the Podman suite.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: passed. Cargo reports the
	existing future-incompatibility notice for `proc-macro-error2` 2.0.1; no new Clippy warning.
- Focused `rustfmt --check` on all 14 touched Rust files: passed.
- New fixed-seed capsule property test: 256 cases with shrinking, plus full existing property suites.
- New real-Iroh proxy classification regression, admission wire-value contract, pending-writer
	cancellation regression, and sidecar byte-handoff/URL parser regressions: passed.
- Shell syntax: deployment builder and devcontainer setup passed
	without executing privileged setup or deployment commands.
- Changed-runtime `git diff --check`: passed. Whole-worktree check reports only existing trailing
	whitespace in `.github/copilot-instructions.md`, which was not changed by this pass.
- The first full workspace run was interrupted during scheduler tests when terminal tools reused
	the same shell session. It was not counted as complete. The standalone quiet rerun completed.
- No Podman images rebuilt or real Podman/multi-host suite executed in that pass.

## Changed Interfaces

`ReplicaRequest::prepare_execution`, `OwnerSigningKeys`, `ResponseKemKeys`, and `PreparedExecution`
are private CLI interfaces. Proxy envelope acceptance remains a private typed helper around the
existing shared validator. `protocol::agent::AdmissionRefusal` is an additive Rust enum mapping to
the existing reservation reason strings; it does not add or change serialized wire fields. The
runtime trait, service entry points, signed-message versions, and credential lifetimes are unchanged.

The intended behavior changes are refusal-metrics finalization, cancellation during raw EOF
shutdown, preserved sidecar buffered bytes, and standards-based absolute proxy URL parsing.
Absolute proxy URLs now require HTTP(S), reject embedded credentials and malformed input, use the
scheme's default port, and preserve path/query while excluding fragments. HTTPS URL parsing does
not add TLS origination; the existing HTTPS application workflow uses CONNECT.

## Review Coverage and Deliberate Non-Changes

| Code part | Review depth and decision |
|---|---|
| CLI | Detailed create/update preparation and private key interfaces; share capsule preparation but retain distinct admission, update CAS, signing context, and response checks. Existing full-flow and generated capsule checks cover the extraction. |
| Agent | Detailed admission reasons, deployment failure bookkeeping, runtime and encrypted store interfaces; reuse protocol-owned refusal vocabulary. Runtime cleanup and store recovery remain a separate state-machine issue, not a string cleanup. |
| Proxy | Detailed control dispatch, grant/session ownership, metrics finalization and typed acceptance; fix early-return classification and use a local validator context. Keep operation authorization explicit. |
| Scheduler | Interface scan across machine/relay modules, targeted coordination/authorization/clock reads; reuse one existing clock function. Do not merge bounded location, capacity and reconciliation registries with different lifetime and trust semantics. |
| Sidecar | Interface scan, detailed connection/stream handoff and HTTP proxy parsing; fix lost prefetched bytes and URL default-port/query handling. Broad connection context redesign remains deferred. |
| Protocol and crypto | Interface scan, targeted signatures, capsule, credential, manifest, resource and replay validation; centralize admission reason semantics without changing serialized fields. Distinct signature domains and operation validators remain separate. |
| Iroh support | Detailed raw duplex supervision, cancellation and identity boundaries; fix uncancellable EOF shutdown. Keep endpoint/relay provisioning separate from application authorization. |
| Axum support | Detailed token-bucket and callback lock scope; existing bounded cache and callback outside lock are retained. Rate limiting is not admission authorization. |
| Metrics | Detailed operation timers, histogram snapshots, catalog and recording interfaces; remove a redundant float trait, retain closed per-component vocabulary and existing model properties. |
| Scripts/configuration | Targeted deployment/devcontainer reads; preserve image tooling. The privileged development container is an explicit local setup, not a tenant isolation claim. |
| Tests | Inventory and interface scan with focused fixture/property reads; reuse existing tests and add regressions near each changed boundary. Real Podman execution is not implied by default Cargo tests. |

This is inventory-wide scanning plus targeted detailed review, not a line-by-line audit of every
source file. No file is marked security-certified merely because it appears in the inventory.

## Remaining Findings

The following are historical review findings. Current status and completion belong only to the
[OpenSpec change tasks](changes/complete-poc-hardening/tasks.md), not this list.

- **Failed-deploy tracking:** Podman may create runtime state before returning an error. The
	existing runtime API returns its identifier only on success; robust cleanup needs a recoverable
	failure/cleanup interface and retained accounting. This pass does not resolve that known defect.
- **Error classification:** admission literals now have a single protocol-owned typed vocabulary,
	but the unchanged wire still carries text. Agent/proxy/sidecar metrics still classify some
	`anyhow` errors by text; migrating error types and metrics taxonomy needs its own regression scope.
- **Sidecar connection context:** several private/internal functions accept many arguments. A
	cohesive connection context may help, but should not become a general service context or change
	exported interfaces just to remove lint suppressions.
- **HTTP proxy admission:** request-line/header reading and listener task spawning lack the same
	explicit admission bounds used on Iroh streams. The buffering/URL fixes do not claim to harden
	this listener against hostile clients; add whole-header byte/time limits and task admission in a
	dedicated hardening change.

## Guardrails

No new service framework, shared mutable scheduler workload state, proxy persistence, consensus,
automatic scaling, or background controller. Public wire compatibility and authorization checks
remain intact. The known failed-deploy tracking defect is not declared fixed by deduplication.
Tests, not line count, decide whether an extraction is useful. Similar code with different security
semantics need not be merged. Inventory-wide scanning and targeted detailed review are reported
separately; no comprehensive security certification is implied.