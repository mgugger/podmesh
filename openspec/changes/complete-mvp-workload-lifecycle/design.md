## Context

See `proposal.md` for motivation. The current deployment path already separates owner authority,
scheduler relay, agent execution, and proxy/sidecar traffic, but it treats apply as create-only,
uses placeholder placement resources, and relies on client normalization for invariants the agent
must enforce. The scheduler must remain ephemeral and blind throughout this change.

The stable identifiers already provide the required update identity:

- deployment id: owner key plus workload name
- workload id: owner key plus workload name plus replica index
- revision id: canonical execution manifest

The catalog already stores one receipt and target agent per replica and is written incrementally,
which can be extended to track partial updates without introducing a central workload database.

## Goals / Non-Goals

**Goals:**

- Make repeated apply idempotent and update changed revisions in place.
- Keep replica placement fixed during an MVP update.
- Preserve end-to-end owner authorization and scheduler blindness.
- Make capacity selection describe the actual sidecar-injected workload.
- Rebuild owner-side catalog state from agent authority through any scheduler.
- Keep the service mesh mandatory and maintain its expiring authority.
- Enforce runtime and persistence boundaries at the agent.

**Non-Goals:**

- Changing replica count, automatic scaling, rolling-strategy configuration, or surge replicas.
- Moving a replica to another agent during update.
- Recovering workloads after loss of an agent or its durable keys.
- Scheduler-held workload state, desired-state controllers, or background self-healing.
- Backward compatibility for existing catalogs or wire records.

## Decisions

### Update is a revision compare-and-swap on the existing agent

`podctl` loads or reconciles the stable deployment catalog before apply. A new deployment follows
the existing select, admit, and deploy path. An existing deployment requires the same replica count
and sends each placement an owner-signed update naming:

- workload and deployment identity
- current target agent
- expected revision from the last verified receipt
- requested revision
- fresh encrypted execution specification
- response KEM key, issue time, expiry, and nonce

The agent accepts when its stored revision equals the expected revision. If it already equals the
requested revision, it returns an idempotent success. Any other revision is a conflict.

This is preferred over treating deploy as implicit replacement because an explicit update domain
prevents cross-operation signature reuse and makes stale writers detectable.

### Replicas update sequentially and catalog progress is per replica

`podctl` updates replicas in index order and saves the catalog after every verified response.
Successful replicas advance their receipt and revision; failed replicas retain the previous receipt.
A retry skips replicas already at the requested revision.

Sequential replacement avoids taking every replica down simultaneously without introducing surge
placement or rollout policy. Single-replica updates may have downtime; zero-downtime update is not
an MVP guarantee.

### Agent replacement is persisted as a recoverable transition

The stored row gains an update transition containing the previous signed grant and the requested
grant. Before invoking Podman, the agent persists the transition. After successful replacement it
persists the new active record and returns the receipt.

On restart:

- an active row reconciles the recorded revision
- an update transition inspects the runtime and completes or reports the requested revision
- an unreadable row is isolated without aborting the remaining scan

The transition prevents an agent crash between Podman replacement and database commit from making
the resulting revision unknowable. It does not provide remote recovery.

### Admission for update accounts for replacement rather than addition

Update admission compares requested resources with the workload's current reservation. Capacity is
calculated as total committed resources minus the existing workload plus the requested workload.
The reservation binds the existing workload id and expected revision. This permits a replacement
that fits after releasing the old allocation while refusing an update that would exceed aggregate
capacity.

### Placement criteria are supplied by podctl and signed by the scheduler

The selection HTTP request carries bounded numeric CPU, memory, and storage criteria plus bounded
capabilities. `podctl` derives them after canonicalization, security validation, resource defaults,
and sidecar defaults. The scheduler validates the HTTP values and copies them into its signed query.

If admission reports a capacity race, `podctl` excludes that agent and retries up to a named bound.
Trust rejection, malformed offers, and cryptographic failures are not treated as capacity races.

### Agent manifest validation is an explicit allowlist

Production execution accepts exactly one `Pod` or `Deployment` pod-bearing document, normalized to
one runtime pod, plus the explicitly supported Service and Ingress documents used for route
extraction. Other controllers, persistent storage objects, Secret objects, and unknown kinds are
rejected.

The agent repeats canonicalization and resource measurement; it never assumes `podctl` performed
them. Ephemeral containers are rejected for the MVP because Podman behavior and resource semantics
are not part of the supported contract.

### Reconciliation is a bounded ephemeral scheduler operation

The reached scheduler signs and gossips a reconciliation coordination record containing the
owner-signed request and a signer-bound reply endpoint. Each admitted scheduler fans the original
owner request to its local attachments and returns each sealed agent answer directly to the
coordinator. The coordinator tracks only a bounded pending request, deduplicates by authenticated
agent endpoint, reports unreachable scheduler or agent scope, and discards the state at expiry.

Agent answers include the current KEM public key, signing key, endpoint record, workload summary,
current signed receipt, and service-mesh authority metadata needed to reconstruct lifecycle
placements. The entire answer is sealed to the owner.

This is preferred over scheduler indexing because it preserves the stateless trust boundary and
returns current agent authority rather than cached claims.

### Service-mesh authority renews through an update-shaped control operation

Proxy grants are renewed directly with proxies. Per-workload credentials and current proxy records
reach replicas through the same revision-bound update mechanism, even when the application manifest
is unchanged. Credential-only updates preserve workload identity and routing keys.

The production CLI and agent always require sidecar configuration. Tests may construct an explicit
in-process execution mode without sidecar injection; the bypass is not encoded in tenant-controlled
manifest data and is not enabled by production defaults.

### Store permissions and runtime concurrency are enforced at creation boundaries

The agent creates the state directory with `0700` and the database file with `0600` before the
database can expose content. Store iteration returns per-key results so corrupt entries can be
handled independently.

All runtime subprocess operations share a configurable semaphore and named timeout. Control
handlers also bound dispatch and response writes so the advertised operation timeout covers the
complete request.

## Risks / Trade-offs

- **[Podman replacement is not transactionally atomic]** → Persist an explicit transition, update
  replicas sequentially, reconcile the observed runtime after restart, and document possible
  single-replica downtime.
- **[Mesh-wide reconciliation can amplify one unauthenticated HTTP call]** → Retain per-peer rate
  limits and add strict scheduler, agent, response-size, concurrency, and pending-request bounds.
- **[Agent answers contain enough data to rebuild control authority]** → Seal the complete response
  to the requesting owner and require owner signatures and bounded freshness on requests.
- **[Credential renewal can diverge across proxies or replicas]** → Report per-target outcomes and
  retain existing unexpired authority until replacement authority is confirmed.
- **[Resource races remain possible after a valid offer]** → Use bounded client retry; the agent
  remains the only authority that commits capacity.
- **[Strict document allowlisting rejects manifests accepted today]** → No backward compatibility is
  promised; return precise violations and document the supported MVP manifest subset.

## Migration Plan

1. Introduce new update, reconciliation, catalog, and persisted-row versions without compatibility
   decoding.
2. Land strict agent validation and resource-aware placement before enabling update.
3. Enable update and reconciliation only after their protocol and partial-failure tests pass.
4. Add service-mesh renewal and then shorten documentation claims to the guarantees covered by the
   end-to-end suite.
5. Existing development state and catalogs may be removed and recreated; production migration is
   not required before the first release.

Rollback consists of stopping the new binaries and clearing pre-release catalogs and agent state.
Because no released compatibility contract exists, mixed-version operation is not supported.
