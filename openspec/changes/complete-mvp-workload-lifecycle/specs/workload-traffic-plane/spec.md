## ADDED Requirements

### Requirement: The workload service mesh SHALL be mandatory for production workloads

Every production workload replica SHALL run with the injected sidecar and SHALL receive validated
proxy endpoint records, a tenant-scoped relay credential, an owner-signed workload credential, and
the routes derived from its manifest.

#### Scenario: Agent cannot prepare sidecar metadata

- **WHEN** required service-mesh material is missing or invalid
- **THEN** the agent refuses deployment or update before starting the application pod

### Requirement: Traffic-plane authority SHALL be renewable

`podctl` SHALL be able to renew proxy grants and the workload credential for running deployments
before expiry without changing workload identity or placement. Renewal SHALL preserve owner,
routing-key, proxy-endpoint, and agent bindings.

#### Scenario: Long-running workload approaches credential expiry

- **WHEN** a deployment's proxy grant or workload credential enters its renewal window
- **THEN** the owner can issue fresh bounded credentials
- **AND** replicas continue registering and reconnecting through authorised proxies

#### Scenario: Renewal partially fails

- **WHEN** only some proxies or replicas accept renewed authority
- **THEN** the command reports every failed target
- **AND** existing unexpired authority remains usable until its original expiry

### Requirement: Service-mesh bypass SHALL be test-only

An implementation MAY provide an explicit bypass for isolated scheduler and agent tests, but the
bypass SHALL NOT be enabled by default or accepted through an untrusted workload manifest.

#### Scenario: Tenant requests bypass in its manifest

- **WHEN** a tenant adds a manifest field or annotation requesting no sidecar
- **THEN** a production agent ignores or refuses that request

