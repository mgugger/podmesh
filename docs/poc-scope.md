# Podmesh PoC Scope

Podmesh is a greenfield proof of concept for single Kubernetes-style workloads with fixed replicas,
client-driven placement, stateless scheduler coordination and identity-authorized control/traffic.
The [OpenSpec capabilities](../openspec/specs/) own requirements; this document explains their scope.

## Security Boundaries

- **End-to-end encryption:** workload specifications and targeted lifecycle requests/responses are
  encrypted between owner and selected agent. Scheduler relays cannot decrypt or forge them.
  Resource requirements, agent identities, exclusions and candidate counts remain public metadata.
- **Statelessness:** schedulers hold no authoritative or durable workload state. Agents retain local
  records and keys, owners retain keys/catalogs, and proxies hold volatile grants.
- **Zero trust:** guarantees are scoped to owner-agent control and sidecar-proxy authorization.
  The selected host controls execution plaintext. Shared networking does not isolate application
  ports, and approved proxies can read traffic unless applications protect it end to end.
- **Identity-based authorization:** owner signatures authorize lifecycle operations; bearer workload
  credentials and endpoint-bound proxy grants authorize mesh traffic. Any new owner may request
  capacity; identity validation is not an operator admission or quota policy.
- **Decentralization:** multiple scheduler entry points and client-driven placement need no central
  workload database or leader. There is no consensus, Byzantine availability, bootstrap-free
  discovery, or proof that distinct agent identities represent distinct physical hosts.
- **Replay detection:** duplicate nonces are refused while retained. Cache eviction or restart can
  forget a still-fresh message; signatures, addressing, freshness and authorization remain required.
  This does not provide exactly-once execution.

## Accepted Limits

One normalized Pod or Deployment per replica, plus supported Service/Ingress documents. No workload
clustering, desired-state controller, autonomous relocation/scaling, persistent volumes, UDP routing
or mixed-version compatibility. External ingress is plain HTTP. Host/Podman and configured bootstrap
remain trusted boundaries. Traffic proxies use their own network reach on a tenant's behalf.

Proxy grants may remain in memory; restart can require `podctl cert grant-proxy`. Explicit apply
renews credentials in their renewal window. Complete agent/key loss is unrecoverable. No production
latency, availability or recovery SLO is promised.

## Verification

Use ordinary unit, integration, adversarial and property tests with locked dependencies. CI runs
formatting, Clippy, workspace tests and dependency policy directly. Real Podman execution is optional
and requires current images and a socket; multiple local containers do not prove multi-host behavior.
Record commands and limitations with the relevant OpenSpec change, not a separate certification system.

## Failure Recovery

The agent records the planned runtime identity and charges resources before creating a pod. Failed
or cancelled creates remain owner-visible as `cleanup-required` until safe cleanup and durable
record removal complete. Other mutations for that workload are refused while an operation runs;
unrelated workloads remain independent. Confirmed cleanup failures can be retried with owner delete
or at agent restart, without silently redeploying an uncommitted create.

Remote Podman timeouts are different: killing a local CLI does not prove the remote operation
stopped. The agent conservatively retains the record and charge and reports operator repair
required; it does not automatically clear uncertain Podman records from a not-found observation.
No automatic repair/force-clear command is provided for this PoC. Do not delete the whole agent
database to free one pending reservation: investigate the target on the host and preserve sibling
records. Existing active/update records remain readable during coordinated upgrades.

The optional local HTTP proxy now bounds request heads to 16 KiB and 64 headers, applies a five-second
head deadline, caps pre-tunnel handlers at 64, and bounds queue/error writes to one second. These
limits do not change the separate authorization and raw-stream limits or imply global DoS protection.
The [hardening tasks](../openspec/changes/complete-poc-hardening/tasks.md) record verification and the
remaining multi-host demonstration.