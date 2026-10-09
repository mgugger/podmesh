# Accepted Decisions

Migrated on 2026-09-08 from the approved AI-SDLC requirements, application/unit designs, user scope
clarifications, current source specs, and reviewed implementation. This is an index of rationale;
the linked capability specifications remain normative. Superseding a decision requires an explicit
OpenSpec change, not silently editing an operator guide or historical approval.

| ID | Accepted decision and rationale | Requirement owner |
|---|---|---|
| D-01 | Zero trust is scoped to owner-agent control and sidecar-proxy authorization. Hosts execute plaintext and remain trusted; an open mesh is not hostile-host isolation or Byzantine availability. | [Message security](specs/message-security/spec.md), [agent](specs/agent/spec.md) |
| D-02 | End-to-end encryption protects targeted owner-agent control. Iroh encrypts sidecar/proxy transport, not application bytes from the terminating proxy. Public discovery/placement metadata and external HTTP ingress remain visible. | [Message security](specs/message-security/spec.md), [traffic](specs/workload-traffic-plane/spec.md) |
| D-03 | Owners approve agent signing keys before encryption; self-signed offers alone cannot establish recipient trust. `--trust-any-agent` is an explicit opt-out, not the default guarantee. | [CLI](specs/podctl-cli/spec.md), [scheduler](specs/scheduler/spec.md) |
| D-04 | Proxy trust is explicit persisted TOFU binding normalized origin, endpoint ID and signing key, before granting or injection. Typed `trusted_agents` retains legacy agent lines; `podctl cert` owns trust/list/remove and explicit `--replace`. Bootstrap reads are bounded. | [CLI](specs/podctl-cli/spec.md) |
| D-05 | Every workload control operation/direction uses the shared canonical typed envelope. Signature domains, transport sender, recipient, version, algorithm, freshness and nonce checks stay centralized; application signing keys are verified per envelope, not session-pinned. | [Message security](specs/message-security/spec.md) |
| D-06 | Replay state is bounded, process-local and evicting. Availability at saturation takes precedence over retaining every nonce; eviction/restart permits duplicates within freshness bounds. No unconditional replay prevention or exactly-once promise. | [Message security](specs/message-security/spec.md), [agent](specs/agent/spec.md) |
| D-07 | Workload credentials are bearer authority scoped to owner/routing identity; proxy grants bind the authenticated proxy endpoint. They are not proof the sidecar holds the owner private key. No immediate revocation or general attenuation policy is implemented. | [Traffic](specs/workload-traffic-plane/spec.md) |
| D-08 | Client-driven fixed-count replica placement and bounded signed discovery avoid a central workload database or leader. Distinct agent IDs do not attest distinct hosts. Schedulers retain temporary coordination and node trust state, not authoritative workloads. | [Scheduler](specs/scheduler/spec.md), [CLI](specs/podctl-cli/spec.md) |
| D-09 | Agents own Podman, admission/accounting and encrypted local records; owners keep keys/catalogs. Explicit agent-backed catalog discovery and local restart recovery are not desired-state clustering or self-healing. Complete agent/key loss is unrecoverable. | [Agent](specs/agent/spec.md), [CLI](specs/podctl-cli/spec.md) |
| D-10 | One normalized Pod/Deployment per replica, plus the supported Service/Ingress subset. Fixed-placement sequential updates retain partial progress; scaling, relocation, persistent volumes and other controllers are excluded. | [Agent](specs/agent/spec.md), [CLI](specs/podctl-cli/spec.md) |
| D-11 | Proxy/sidecar traffic is decoupled from scheduler/agent processes. Shared workload networking, plain external HTTP, TCP-only egress and trusted bootstrap are accepted PoC limits. Network isolation is out of scope, not impossible across hosts. | [Traffic](specs/workload-traffic-plane/spec.md), [local deployment](specs/local-deployment/spec.md) |
| D-12 | Proxy grants may stay in memory; restart can require manual grant reposting. Credential renewal remains explicit through apply within its renewal window. This accepted simplicity does not excuse untracked failed-deploy execution. | [Traffic](specs/workload-traffic-plane/spec.md), [CLI](specs/podctl-cli/spec.md) |
| D-13 | Open scheduler access and self-created owner identities have no operator admission policy or per-owner workload quota. Bounds limit amplification but are not access control; egress uses the granted proxy's network reach. | [Scheduler](specs/scheduler/spec.md), [agent](specs/agent/spec.md), [traffic](specs/workload-traffic-plane/spec.md) |
| D-14 | Optional dedicated metrics listeners use shared fixed vocabularies and finite cardinality, never raw identities/secrets. Metrics cannot change business authorization. Sidecar metrics configuration stays outside signed tenant metadata. | [Agent](specs/agent/spec.md), [traffic](specs/workload-traffic-plane/spec.md) |
| D-15 | Property testing remains enabled for applicable invariants: structured generators, 256 cases by default, shrinking, reproducible seeds, round trips, idempotence and stateful/oracle models where useful. Replaces loading the retired PBT extension. | [Development workflow](specs/development-workflow/spec.md) |
| D-16 | Superseded 2026-09-09 by D-22: the dedicated release-tooling architecture was removed, not certified complete. | [Scope reduction](changes/simplify-greenfield-verification/proposal.md) |
| D-17 | Superseded 2026-09-09 by D-22: the ten-gate process and optional fuzz/soak campaigns were removed, not passed. | [Scope reduction](changes/simplify-greenfield-verification/proposal.md) |
| D-18 | Coordinated upgrades may reject unknown wire/store versions; no rolling or mixed-version compatibility. Preserve existing wire formats for cleanup unless a requested fix explicitly requires change. | [Local deployment](specs/local-deployment/spec.md), [message security](specs/message-security/spec.md) |
| D-19 | Security/Resiliency extension packs were disabled, not the project-specific security/bounds requirements. No automatic stage approval or heavyweight framework is inherited. Small cohesive interfaces and shared owning abstractions matter more than arbitrary file-size limits. | [Development workflow](specs/development-workflow/spec.md) |
| D-20 | OpenSpec is the sole active requirements/decision/task system. Keep migrated decisions and result summaries, unfinished work and operator projections; the redundant AI-DLC tree was removed on 2026-09-09, superseding the initial retention decision. | [Development workflow](specs/development-workflow/spec.md) |
| D-21 | Preserve AI-DLC's adaptive planning, structured clarification, meaningful approval checkpoints, incremental progress, verification/review and session continuity using OpenSpec artifacts. Approval of a presented plan carries forward; small clear fixes need no ceremony, and completion is distinct from user acceptance. Accepted 2026-09-09. | [Development workflow](specs/development-workflow/spec.md), [workflow change](changes/retain-adaptive-openspec-workflow/design.md) |
| D-22 | Greenfield PoC verification uses normal unit/integration/adversarial/property tests, lint and optional Podman/multi-host execution. Remove fuzzing, soak, dedicated release evidence, SBOM/bundles/baselines and generic certification obligations. Keep concrete runtime security and useful tests; do not introduce speculative legacy compatibility or enterprise process. Accepted 2026-09-09. | [Development workflow](specs/development-workflow/spec.md), [scope reduction](changes/simplify-greenfield-verification/proposal.md) |

## Migration Traceability

| Former record | Current owner |
|---|---|
| AI-SDLC FR-01/02, FR-10/11 | CLI, scheduler, agent and local-deployment specs; D-08/09/10/18 |
| FR-03/12 and owner-proxy-trust designs | CLI spec; D-03/04 |
| FR-04/05/13 and workload-control-envelope designs | Message-security/traffic specs; D-05/06/07 |
| FR-06/07/08/09 | Traffic, scheduler and agent specs; D-01/02/11/12/13 |
| FR-14 and PBT-01 through PBT-10 | Development-workflow spec; D-15 |
| FR-15/16/18 and release acceptance gates | Dedicated assurance scope retired by D-22; ordinary dependency/security tests remain |
| FR-17 and bounded-observability designs | Runtime metrics requirements; D-14 |
| NFR-01 through NFR-10 | Owning runtime specs plus development-workflow; no separate NFR authority |
| U-04 unfinished baseline and final evidence | Removed by D-22, not completed; historical results do not establish current runtime success |
| Lean-interface code inventory and remaining findings | OpenSpec code inventory and remaining-work change |

The legacy tree was removed on 2026-09-09 after this migration. The decision index, OpenSpec changes
and dated verification summaries retain the useful project record, not every original transcript.
New decisions belong in the relevant change design; update this index only for durable accepted
rationale. It is not a feature backlog or a second requirements ledger.