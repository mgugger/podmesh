# Stateless Workload Placement Scheduler

## Purpose

Keep scheduling independent from workload execution and durable state. The scheduler stores only
signed, expiring agent advertisements in memory and selects one available candidate without
receiving workload requirements, manifests, keys, status, or logs.

## Interfaces

- `POST /api/v1/agents`: validate and register a signed `AgentAdvertisement`.
- `GET /api/v1/agents/select`: return the available advertisement with the lowest coarse load.
- `GET /health`: liveness only.

## Security And Bounds

- Advertisements are self-signed by the advertised Ed25519 `NodeId` and expire quickly.
- Registry size, request body size, advertisement lifetime, and exclusion list size are bounded.
- A newer advertisement cannot be replaced by an older replay.
- The registry is intentionally lost on scheduler restart; agents republish automatically.

## Non-Goals

- Workload execution, persistence, decryption, status, logs, deletion, or recovery.
- Durable cluster state or Kubernetes-compatible APIs.
- Replica placement and recovery in the initial implementation.