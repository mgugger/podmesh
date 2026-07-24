# Biscuit Capability Tokens for Proxy And Sidecar Authorization

## Problem

NodeCert and handshake checks establish proxy identity and tenant binding, but sidecar registration,
ingress, and egress still need explicit least-privilege authorization.

## Proposed Solution

Add offline-verifiable, tenant-issued Biscuit tokens to:

- Sidecar registration, bound to tenant, workload, and sidecar peer.
- Ingress forwarding, bound to workload and allowed path prefixes.
- Egress tunneling, bound to workload and allowed destination host/port.

Identity verification remains mandatory and precedes token authorization. Missing, expired, or
unverifiable tokens fail closed. This change has no scheduler or execution-agent responsibility.

## Non-Goals

- Global online token introspection.
- Workload key release or recovery.
- Replacing NodeCert identity.