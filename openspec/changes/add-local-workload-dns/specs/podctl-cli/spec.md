## MODIFIED Requirements

### Requirement: podctl SHALL mint a workload credential for every deployment

On deployment, podctl SHALL automatically provision owner-rooted replica-scoped enrollment authority, encrypted to the selected agent. That authority SHALL constrain delegated identity issuance to the intended tenant, workload and replica/incarnation policy. The owner private key SHALL remain with the owner; sidecar identity keys SHALL be generated locally rather than by the CLI. Workload sessions SHALL require the resulting endpoint-bound credentials.

#### Scenario: Every deployment carries a fresh credential
- **WHEN** podctl deploys a workload
- **THEN** execution configuration carries fresh owner-authorized enrollment scope for its selected replica and issuer
- **AND** no bearer workload token is sufficient for session authorization

#### Scenario: The credential is not visible outside the tenant
- **WHEN** deployment configuration travels through schedulers
- **THEN** enrollment authority remains inside the encrypted owner-agent payload

## ADDED Requirements

### Requirement: podctl SHALL automate initial workload peer exchange
After replicas enroll, podctl SHALL obtain their current authorized endpoint records through owner-agent control and deliver bounded discovery seeds through the same path. It SHALL include reachable authorized existing tenant peers when available to avoid isolated per-deployment meshes. It SHALL remain an HTTP client without an Iroh endpoint.

#### Scenario: First three-replica deployment
- **WHEN** three replicas enroll successfully
- **THEN** their seed configuration forms a connected initial discovery topology

#### Scenario: A later service joins an existing tenant
- **WHEN** an authorized existing tenant endpoint is available
- **THEN** new service replicas receive a verified existing-mesh contact

### Requirement: Bootstrap progress SHALL survive partial deployment
The deployment catalog SHALL retain per-replica enrollment/bootstrap outcomes as work completes. A pending first replica SHALL NOT prevent placement of later replicas needed to form the mesh. Retrying apply SHALL resume safe pending bootstrap without orphaning confirmed workloads or silently changing identity authority.

#### Scenario: Apply stops before seed delivery
- **WHEN** a deployment is interrupted after replicas are running
- **THEN** their catalog entries remain inspectable and deletable
- **AND** a later apply can resume bootstrap

#### Scenario: One enrollment times out
- **WHEN** one replica cannot finish enrollment
- **THEN** confirmed replicas remain recorded and the CLI reports the incomplete replica rather than claiming complete readiness

### Requirement: Runtime child renewal SHALL not require owner CLI interaction
While delegated authority remains valid, child credential renewal SHALL proceed without podctl connectivity or manual token/key handling. Expired finite parent authority SHALL remain expired; standing enrollment authority and its revocation limitations SHALL be explicit policy rather than implicit self-extension.

#### Scenario: CLI is offline during renewal
- **WHEN** a running replica has valid delegated renewal authority
- **THEN** it renews its endpoint-bound child credential without the CLI

#### Scenario: Finite parent authority expires while CLI is offline
- **WHEN** no valid delegated authority remains
- **THEN** issuance fails closed and does not manufacture owner authorization
