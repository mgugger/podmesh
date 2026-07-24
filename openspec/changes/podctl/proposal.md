# Encrypted Direct-Agent Workload Client

## Purpose

Make `podctl` the namespace authority and keep complete workload specifications encrypted from all
nodes except the selected execution agent.

## Apply Flow

1. Normalize the complete multi-document manifest and derive full 256-bit namespace-scoped workload
   and content-addressed revision IDs.
2. Request one signed agent advertisement from the stateless scheduler.
3. Apply built-in manifest policy defaults, measure aggregate container limits, add deterministic
   sidecar overhead, and send those limits in an owner-signed admission request encrypted to that
   agent.
4. Encrypt the execution specification with a random DEK and wrap the DEK to the admitted agent.
5. Send an owner-signed deployment grant directly to the agent.
6. Verify and store the encrypted agent receipt locally.

## Lifecycle

Status, logs, and delete use owner-signed commands encrypted directly to the agent address in the
local receipt. Listing reads the local receipt catalog and is non-authoritative.

## Availability

Only one workload replica is supported initially. Loss of the agent and its durable keys/state
requires owner-driven deployment. Replica handoff is deferred.