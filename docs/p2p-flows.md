# Podmesh Network Flows

Podmesh currently uses direct HTTP for scheduling and workload lifecycle, and libp2p QUIC for the
proxy/sidecar traffic plane. Protocol records are transport-neutral so agent HTTP endpoints can be
replaced by Iroh later without changing workload identity or authorization.

## Scheduler And Agent

The scheduler has no libp2p behaviour and no durable state.

```text
agent --POST signed AgentAdvertisement--> scheduler /api/v1/agents
podctl --GET candidate------------------> scheduler /api/v1/agents/select
podctl ==encrypted AdmissionRequest=====> agent /api/v1/admission
podctl ==encrypted DeploymentGrant======> agent /api/v1/deploy
podctl ==encrypted WorkloadCommand======> agent /api/v1/command
```

Agent advertisements reveal only node identity, KEM key, endpoint, coarse load, capabilities, and
expiry. Workload requirements are encrypted to the selected agent and never reach the scheduler.

Each agent can run many workloads. Admission accounts for active workloads and outstanding signed
reservations against:

- `max_workloads`
- total CPU millicores
- total memory bytes
- total storage bytes

The agent persists one encrypted redb row per workload, keyed by the full workload ID. On restart it
reconciles every row independently. Deleting one workload removes only that row and runtime pod.

## Proxy `WorkloadBehaviour`

Defined in `podmesh-proxy/src/p2p.rs`:

- `gossipsub` on `podmesh-workload`
- `handshake_rr` for signed peer handshake and tenant proxy certificate exchange
- `kademlia` for workload-plane discovery
- `proxy_rr` for ingress HTTP forwarding to sidecars
- `manifest_rr` for sidecar manifest record fetches
- `egress_stream` for sidecar egress tunnels
- `registration_rr` for inbound sidecar route registration
- `identify`, relay, and AutoNAT behaviours supplied by the shared swarm setup

The proxy keeps an in-memory routing table populated by authenticated sidecar registrations. It
checks this table before using the manifest-provider discovery fallback.

## Sidecar `SidecarBehaviour`

Defined in `podmesh-sidecar/src/lib.rs`:

- `kademlia` for proxy and provider discovery
- `handshake_rr` for peer identity and tenant proxy certificate verification
- `proxy_rr` for inbound ingress requests
- `manifest_rr` for serving signed manifest records
- `egress_stream` for outbound tunnels
- `registration_rr` for outbound registration with verified proxies

The agent injects the sidecar only after decrypting the workload. Sidecar metadata contains the
namespace, workload identity, complete manifest, and bootstrap proxy address; this metadata is never
sent to the scheduler.

## `podctl` Endpoints

| Command | Network path |
|---|---|
| `apply -f` | scheduler selection, then encrypted agent admission and deployment |
| `delete -f` | owner-signed encrypted command sent directly to receipt agent |
| `get pods` | local receipt catalog only |
| `get pods <id>` | owner-signed encrypted status command to receipt agent |
| `logs <id>` | owner-signed encrypted logs command to receipt agent |
| `convert` | local only |
| `cert grant-proxy` | proxy REST identity endpoints and certificate provisioning |

## Availability Boundary

The scheduler may restart without affecting workloads; agents republish advertisements. Agent
process restart reconciles all workloads from encrypted local state. Remote recovery after loss of an
agent and its durable keys is not implemented. Workloads requiring that guarantee must eventually
use multiple replicas and replica handoff.

## Current Security Boundary

- Complete workload specifications and lifecycle responses are encrypted between `podctl` and the
  selected agent.
- The selected agent necessarily sees plaintext to execute the workload.
- Proxy ingress is L7 HTTP and therefore not confidential from the proxy unless the application uses
  end-to-end TLS or another workload-terminated encrypted protocol.
- Public advertisements and network timing remain observable metadata.