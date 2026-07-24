# Secure Workload Submission And Execution

## Purpose

Execute complete encrypted workloads without exposing plaintext specifications or DEKs to the
stateless scheduler or unrelated agents.

## Actors

- Namespace client (`podctl`)
- Stateless scheduler
- Selected `podmesh-agent`
- Workload runtime and injected sidecar

## Guarantees

- `podctl` encrypts the complete normalized workload before transmission.
- The scheduler receives only signed, expiring agent advertisements.
- Admission and deployment are encrypted directly to the selected agent.
- Owner signatures bind the target agent, reservation, workload and revision IDs, ciphertext,
  wrapped DEK, expiry, and nonce.
- Each agent can execute many independent workloads within aggregate count and resource limits.
- Local encrypted records allow agent-process restart reconciliation.

## Availability Boundary

Remote recovery after loss of an agent and its durable node keys is not implemented. A workload
requiring that guarantee must eventually request multiple replicas and replica handoff.

## Non-Goals

- Global desired-state storage
- Kubernetes API compatibility
- Billing or cross-region optimization
- Confidential execution from the selected host without a TEE