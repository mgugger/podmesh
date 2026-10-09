# podctl-cli Specification

## Purpose

`podctl` is the namespace owner's command line tool. It is a plain HTTP client with no Iroh
endpoint: it reaches any scheduler over HTTP and the scheduler relays control traffic to agents.
It owns the tenant Ed25519/X25519 keys, decides replica placement, and encrypts every workload
payload so that no intermediary can read or forge it.

## Requirements

### Requirement: podctl SHALL report what it does and who it is

Flags SHALL do what they say. The output format flag SHALL select the rendering, `--force` SHALL have
an effect, and the operator SHALL be able to print the namespace identity this installation deploys
under.

#### Scenario: The output format is honoured

- **WHEN** the operator asks for JSON or table output
- **THEN** that format is produced

#### Scenario: The operator can learn their namespace identity

- **WHEN** the operator runs the identity command
- **THEN** the base64 owner public key is printed

### Requirement: podctl SHALL NOT operate an Iroh endpoint

`podctl` SHALL communicate exclusively over HTTP with a scheduler chosen by the operator. It SHALL
NOT bind an Iroh endpoint, join gossip, or dial agents directly.

#### Scenario: Applying a manifest against a reachable scheduler

- **WHEN** the operator runs `podctl --api-url <scheduler> apply -f <manifest>`
- **THEN** every request is an HTTP request to `<scheduler>`
- **AND** no Iroh endpoint is created in the `podctl` process

#### Scenario: Any scheduler is interchangeable

- **GIVEN** three schedulers in the mesh
- **WHEN** the operator points `--api-url` at any one of them
- **THEN** the command succeeds with identical results, because schedulers are stateless

### Requirement: podctl SHALL decide which agents may read its workloads

A capacity offer is self-signed and names the KEM key the workload is sealed to, so whoever answers a
selection request chooses who can read the plaintext. `podctl` SHALL therefore check every offer
against the owner's own list of trusted agent signing keys before producing any ciphertext.

When no such list is configured, `podctl` SHALL refuse to deploy and say so, unless the operator
explicitly opts out with `--trust-any-agent`. Defaulting to accepting any agent would silently hand
the scheduler the ability to read every workload.

The trusted agent key of the agent that accepted a deployment SHALL be recorded, and later lifecycle
commands SHALL name it, so a scheduler cannot re-aim them at another agent.

#### Scenario: An untrusted agent is refused before anything is encrypted

- **WHEN** the mesh offers an agent whose signing key is not in the trusted list
- **THEN** `podctl` aborts and no ciphertext is produced

#### Scenario: Opting out is explicit and visible

- **WHEN** no trusted agent list is configured and `--trust-any-agent` is not passed
- **THEN** `podctl` refuses to deploy and explains how to configure trust

### Requirement: podctl SHALL decide replica placement

`podctl` SHALL read `spec.replicas` or the `podmesh.io/replicas` annotation, pin the manifest it
ships to a single pod, and request one agent per replica. Each request SHALL exclude the agents
already selected. The scheduler SHALL NOT decide the replica count or fan a deployment out on its
own. Candidate counts reveal how many replicas remain to place; replica count secrecy is not a
guarantee. Distinct agent endpoint identities do not attest distinct physical hosts.

#### Scenario: Placement asks for as much choice as it needs

- **WHEN** `podctl` selects an agent for a deployment with several replicas
- **THEN** it asks the scheduler to collect enough offers to place the remaining
  replicas on distinct agents

#### Scenario: Three replicas land on three distinct agents

- **GIVEN** a manifest with `spec.replicas: 3` and at least three agents with capacity
- **WHEN** the operator applies it
- **THEN** `podctl` issues three `GET /api/v1/agents/select` calls
- **AND** each call after the first passes `?exclude=` listing the previously selected agents
- **AND** `podctl` admits and deploys against each returned agent itself

#### Scenario: Fewer agents than replicas

- **GIVEN** a manifest requesting more replicas than there are eligible agents
- **WHEN** a selection request cannot be satisfied
- **THEN** `podctl` reports the shortfall and the already-deployed replicas remain running
- **AND** those replicas are still listed, inspectable and deletable

### Requirement: The deployment catalog SHALL survive partial failure

`podctl` places replicas itself and the mesh keeps no owner-side index, so the local catalog is the
only handle on a running deployment. It SHALL be written after each replica is confirmed, not once at
the end, and SHALL be written atomically. A replica that could not be deleted SHALL stay in the
catalog so the command can be retried.

#### Scenario: A deployment that fails midway leaves no orphans

- **GIVEN** a three-replica deployment whose third replica cannot be placed
- **WHEN** the command fails
- **THEN** the first two replicas are recorded and can be deleted

#### Scenario: An unreachable agent does not hide the healthy replicas

- **WHEN** one replica's agent does not answer a status or logs command
- **THEN** every replica is still attempted and the per-replica outcome is reported

#### Scenario: A failed delete keeps the remaining replicas addressable

- **WHEN** some replicas delete and others fail
- **THEN** the deleted ones are dropped from the catalog and the rest remain, and the command reports
  which failed

### Requirement: podctl SHALL find workloads its local catalog lost

`podctl` SHALL ask the mesh which workloads it is running for this owner, and
SHALL mark any the local catalog does not know about. The catalog is the only
index of where replicas were placed, so one that was lost, overwritten, or
written on another machine leaves workloads running that nothing local can
address.

The report SHALL name the agents that did not answer, so an incomplete view is
visible as incomplete rather than being read as "the workload is gone".

#### Scenario: A lost catalog does not lose the workloads

- **GIVEN** a deployment whose local catalog entry has been removed
- **WHEN** the operator asks the mesh
- **THEN** every replica is still reported, and flagged as orphaned

### Requirement: podctl SHALL address a deployment without its manifest

A deployment SHALL be addressable by deployment id or by workload name, not only by the manifest file
it was applied from.

#### Scenario: Deleting without the original file

- **WHEN** the operator no longer has the manifest
- **THEN** `podctl delete <deployment>` still removes it

### Requirement: podctl SHALL own the tenant keys and encrypt every payload

`podctl` SHALL load or create the owner Ed25519 signing keypair and X25519 KEM keypair under
`~/.podmesh/`, or under `PODMESH_KEY_DIR` when set, with `0600` permissions. Every admission request,
deployment grant, and lifecycle command SHALL be signed by the owner signing key, addressed to the
selected agent, and encrypted to that agent's KEM key.

One installation holds one namespace identity. Multiple tenants on one machine are a non-goal for the
first release; deployments are multi-tenant, the CLI is not.

#### Scenario: Scheduler cannot read a relayed payload

- **WHEN** `podctl` posts an encrypted deployment grant to the scheduler
- **THEN** the scheduler relays opaque bytes to the agent
- **AND** the scheduler can neither decrypt nor re-sign the payload

#### Scenario: Lifecycle commands require the owner key

- **GIVEN** a workload deployed by one owner key
- **WHEN** a different key issues a status, logs, or delete command
- **THEN** the agent rejects it

### Requirement: podctl SHALL mint owner-signed proxy grants

Before deploying, `podctl` SHALL mint a bounded, expiring Biscuit grant for each configured proxy
and POST it to that proxy's `POST /api/v1/proxy_grant`. The grant SHALL bind the tenant owner
public key, the proxy endpoint identifier, an issue time, an expiry, and a unique token id. Its
lifetime SHALL NOT exceed `MAX_PROXY_GRANT_LIFETIME_SECS`, and every default SHALL be within that
bound so the command works as shipped.

The proxy endpoint a grant names SHALL be taken from the proxy's signed `EndpointRecord`, not from an
unauthenticated field, so an on-path attacker cannot obtain an owner-signed grant for an endpoint it
controls.

The signed record is self-consistent but not independently authoritative: whichever party answers a
configured plain HTTP URL chooses the signing key. Before minting a grant, `podctl` SHALL therefore
require an existing owner-local binding from the proxy's canonical HTTP or HTTPS origin to both its
Iroh endpoint id and signing key. Missing or mismatched trust SHALL be refused before owner signing
or grant submission.

#### Scenario: A grant names only an attested endpoint

- **WHEN** a proxy advertises an endpoint id that disagrees with its signed record
- **THEN** `podctl` refuses to mint a grant

#### Scenario: An untrusted proxy receives no owner authority

- **WHEN** a configured proxy has no owner-local identity binding
- **THEN** `podctl` refuses before minting or posting a grant
- **AND** it directs the owner to the explicit proxy trust command

#### Scenario: Proxy presents the grant to a sidecar

- **WHEN** a proxy handshakes with a tenant's sidecar
- **THEN** it presents the grant minted by that tenant's owner key
- **AND** the sidecar verifies the signature, the tenant owner, the proxy endpoint, and the expiry

#### Scenario: Grant for a different proxy is rejected

- **WHEN** a proxy presents a grant whose proxy endpoint does not match its own endpoint id
- **THEN** the sidecar refuses the connection

### Requirement: podctl SHALL mint a workload credential for every deployment

Only `podctl` holds the namespace owner's private key, so only `podctl` can produce proof that a pod
belongs to a tenant. On each deploy it SHALL mint an owner-signed, expiring Biscuit binding the
tenant owner key and the deployment's routing key, and place it in the execution specification, which
is encrypted to the selected agent. The credential SHALL NOT be sent in the clear and SHALL NOT be
reused across workloads.

Without it a proxy could only take a caller's word for its tenant, and the owner's public key is not
a secret.

#### Scenario: Every deployment carries a fresh credential

- **WHEN** `podctl` deploys a workload
- **THEN** the execution specification carries a credential bound to that owner and routing key

#### Scenario: The credential is not visible outside the tenant

- **WHEN** the deployment travels through a scheduler to an agent
- **THEN** the credential is inside the owner-encrypted payload, so no scheduler observes it

### Requirement: podctl SHALL bootstrap proxy configuration over HTTP

When `PODMESH_PROXY_URL` is set, `podctl` SHALL fetch each proxy's signed `EndpointRecord`, its own
tenant's derived workload relay token, and the relay CA certificate from that proxy's REST API. It
SHALL name its tenant in the request and SHALL NOT receive the mesh secret those tokens derive from.
All listed proxies SHALL return the same token for that tenant; disagreement means different mesh
secrets and SHALL be a hard error. Values supplied programmatically SHALL take precedence over
bootstrapped ones; `PODMESH_PROXY_URL` SHALL take precedence over manifest annotations and over the
other ambient environment variables.

Every configured proxy SHALL match an existing owner-local origin, endpoint-id, and signing-key
binding before its endpoint record, relay token, or relay CA is accepted. `podctl` SHALL read each
endpoint-record and relay-bootstrap response with an independent 64 KiB streaming limit and a
10-second request timeout. Ordinary apply and grant operations SHALL never create or replace trust.

#### Scenario: Proxies disagree on the relay token

- **GIVEN** two proxies holding independent mesh secrets
- **WHEN** both are listed in `PODMESH_PROXY_URL`
- **THEN** `podctl` aborts rather than shipping a sidecar that can reach only one relay

#### Scenario: Bootstrapped records are verified

- **WHEN** `podctl` receives a signed `EndpointRecord` over plain HTTP
- **THEN** it verifies the signature and expiry before using it
- **AND** a tampered record is discarded

#### Scenario: A substituted proxy identity is refused

- **GIVEN** a proxy URL bound to one endpoint id and signing key
- **WHEN** that URL later serves a different endpoint id or signing key
- **THEN** `podctl` refuses every bootstrap value from it
- **AND** it does not replace the binding during apply

### Requirement: podctl SHALL manage proxy trust explicitly

Proxy trust SHALL be changed only through `podctl cert trust-proxy`, `list-proxies`, and
`remove-proxy`. Invoking `trust-proxy` is the explicit non-interactive owner decision: it SHALL print
the full canonical origin, endpoint id, and signing key immediately before committing. Existing
trust SHALL require `--replace`, which prints the old and new identities. Removal SHALL be
idempotent and SHALL NOT remove agent trust.

The existing `trusted_agents` file SHALL become a bounded, versioned typed trust registry. Valid
legacy bare agent-key files SHALL remain readable; the first successful mutation SHALL migrate them
to typed form, preserving unique keys but not comments. On Unix, overly broad permissions SHALL be
narrowed before content is read. Typed state SHALL be committed through a private same-directory
temporary file, file sync, atomic rename, and parent-directory sync.

`PODMESH_TRUSTED_AGENTS` SHALL retain its current per-process override for agent authorization but
SHALL NOT affect persisted proxy trust. Concurrent trust writers are not coordinated; the last
successful complete atomic write wins. A post-rename directory-sync failure SHALL be reported as a
persistence error and the operator SHALL reload before retrying because the complete new file may
already be visible.

#### Scenario: First proxy trust is explicit

- **WHEN** an owner runs `podctl cert trust-proxy --proxy-url <url>`
- **THEN** the command fetches one bounded signed observation
- **AND** prints the complete canonical origin, endpoint id, and signing key
- **AND** atomically persists the binding without minting a proxy grant

#### Scenario: Replacement cannot be implicit

- **GIVEN** an existing proxy binding
- **WHEN** the owner runs the trust command without `--replace`
- **THEN** the command refuses and leaves the binding unchanged

#### Scenario: Legacy trust migrates safely

- **GIVEN** a valid legacy file containing agent keys and comments
- **WHEN** the first proxy trust mutation succeeds
- **THEN** every unique agent key is preserved in typed version 1 state
- **AND** comments are not preserved
- **AND** an older binary refuses the non-base64 version header

### Requirement: CLI tests SHALL exercise owner workflows

CLI changes SHALL use the existing owner-flow tests for affected selection, apply, update,
status/logs, catalog discovery and deletion behavior. Tests SHALL distinguish real container
execution from in-process mocks and SHALL report failure directly through the normal test runner.

#### Scenario: An owner workflow fails

- **WHEN** a tested owner operation fails unexpectedly
- **THEN** its test fails with contextual diagnostics rather than producing a synthetic success
