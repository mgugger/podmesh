# Secure Workload Submission & Execution

## Purpose

Allow clients to submit workloads to the mesh without exposing
plaintext workload specifications or decryption keys to schedulers
or unrelated nodes.

## Actors

- Client
- Scheduler
- Worker Node
- Mesh Peer

## Core Guarantees

- Workload specs are encrypted client-side
- Scheduler cannot decrypt workload payloads
- Worker nodes only decrypt workloads assigned to them
- Decryption keys are never broadcast to the mesh
- Nodes cannot impersonate workload owners

## Constraints

- Must operate over libp2p
- Must tolerate node churn
- Must support multi-tenant isolation
- No centralized trust authority

## Workload Lifecycle

1. Client creates workload spec
2. Client encrypts workload payload
3. Client submits encrypted payload to mesh
4. Scheduler assigns workload without plaintext visibility
5. Authorized worker retrieves encrypted payload
6. Worker receives decryption capability
7. Worker executes workload
8. Results are encrypted before return

## Trust Boundaries

### Scheduler
Can:
- route workloads
- track availability
- coordinate assignments

Cannot:
- decrypt workload contents
- access workload secrets

### Worker Nodes
Can:
- decrypt assigned workloads
- execute workloads

Cannot:
- access unrelated workloads
- impersonate clients

## Open Questions

- How are decryption capabilities delegated?
- Are keys ephemeral or long-lived?
- Is attestation required?
- How are compromised workers handled?
- How is replay prevented?

## Non-Goals

- Billing
- GUI management
- Cross-region optimization
