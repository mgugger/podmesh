## Context

The client is the namespace authority. It must deploy while revealing the complete workload only to
the selected agent and must manage that workload without scheduler-owned state.

## Goals / Non-Goals

**Goals:** full manifest encryption, target-bound grants, full 256-bit identities, direct lifecycle
commands, and deterministic multi-document handling.

**Non-Goals:** authoritative global listing, offline recovery after agent loss, or replica handoff.

## Decisions

- Derive stable `WorkloadId` from namespace key and workload name; derive `RevisionId` from normalized
  complete manifest content.
- Use a random DEK per grant, XChaCha20-Poly1305 for the capsule, and X25519 wrapping to the agent.
- Sign every grant field, including ciphertext, target, reservation, expiry, and nonce.
- Store verified receipts locally with mode `0600`; list is a convenience index, not cluster state.

## Risks / Trade-offs

- [Receipt loss prevents management] -> running workload remains unaffected; receipt backup is an
  owner responsibility until a future encrypted catalog exists.
- [Selected agent rejects stale placement] -> client retries can be added without changing protocol.
- [Namespace key is long-lived] -> delegated operational keys remain a future extension.

## Migration Plan

Remove share/custodian flags and provider discovery. Existing deployments must be reapplied through
the new agent protocol.