## MODIFIED Requirements

### Requirement: The local manifests SHALL expose application metrics only on the pod network

Both rootless and rootful manifests SHALL explicitly enable application metrics with identical
topology: schedulers on 9200-9202, agents on 9210-9212, proxies on 9220-9222, and every injected
sidecar on 9230 in its workload pod. Each runtime SHALL use `PODMESH_METRICS_LISTEN`; agents SHALL
use `PODMESH_AGENT_SIDECAR_METRICS_LISTEN` only as the source they propagate to child sidecars.

The manifests SHALL declare matching TCP container ports and SHALL NOT publish a metrics `hostPort`.
Application metrics SHALL remain distinct from scheduler relay metrics on 9090-9092 and workload
relay metrics on 9100-9102. The sample deployment SHALL add no collector, helper container,
dashboard, alert, trace service, metric volume, queue, or retention system.

#### Scenario: Metrics ports do not collide in shared pod namespaces

- **WHEN** either manifest is parsed
- **THEN** every scheduler, agent, and proxy application-metrics port is unique within its pod
- **AND** no application-metrics port equals an API, ingress, Iroh, relay, or relay-metrics port

#### Scenario: Unauthenticated metrics are not host-published

- **WHEN** either sample stack is started
- **THEN** control-plane application metrics are reachable from the `podmesh` network
- **AND** workload sidecar metrics are reachable from their tenant network
- **AND** no application-metrics port is published to the host

### Requirement: The deploy documentation SHALL describe an end-to-end trial

`deploy/README.md` SHALL document building the images, starting the mesh, pointing `podctl` at it,
configuring which agents the owner trusts, deploying a workload, spreading replicas across agents,
reaching ingress, and tearing the stack down. It SHALL NOT instruct the operator to create any
secret.

It SHALL state plainly what the local stack does not protect: ingress is plain HTTP, the scheduler
API is unauthenticated, the proxy accepts grants from anyone, agent state uses one shared volume,
egress tunnels are not filtered by destination address, and the agents drive a Podman socket that
is equivalent to host control. It SHALL explain on-demand tenant networks, retained unused networks,
and how the local trial makes proxy/control endpoints reachable without attaching workloads to the
control network. Test-only infrastructure attachments SHALL NOT become production runtime behavior.

#### Scenario: Operator follows the README

- **WHEN** an operator follows the documented steps on a clean machine with Podman
- **THEN** they reach a running workload reachable through the proxy ingress port
- **AND** they can list, inspect logs for, and delete it with `podctl`

#### Scenario: Re-running the trial does not orphan the first deployment

- **WHEN** the operator follows the replica-spreading step after the single-replica step
- **THEN** the documentation has them delete the first deployment first
