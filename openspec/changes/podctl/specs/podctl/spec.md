# podctl

## ADDED Requirements

### Requirement: podctl seals workload specifications entirely client-side before submission

podctl MUST encrypt the workload spec and split the DEK before any data reaches the scheduler.

#### Scenario: Applying a workload manifest
- Given a YAML manifest file is provided with `--shares N --threshold K`
- And the scheduler returns N custodian infos from `GET /api/v1/custodians?max=N`
- When `podctl apply` runs
- Then it parses the YAML to a canonical JSON string
- And generates a random 32-byte DEK
- And encrypts the JSON with XChaCha20-Poly1305 using the DEK
- And splits the DEK into N Shamir shares over GF(256)
- And ECIES-wraps each share to the corresponding custodian's X25519 KEM public key
- And signs `postcard(SealedSpec)` with the owner's Ed25519 private key
- And posts `WorkloadSubmission` to the scheduler — the scheduler never sees the plaintext spec or DEK

### Requirement: podctl derives manifest_id as a truncated blake3 hash of the spec JSON

The manifest_id used throughout the system MUST be derived deterministically from the spec.

#### Scenario: Manifest ID derivation
- Given a spec JSON string
- When podctl computes the manifest_id
- Then it is `blake3(spec_json_bytes)[..8].to_lowercase_hex()` — a 16-character hex string
- And the same spec always produces the same manifest_id

### Requirement: podctl persists the owner keypair on disk

podctl MUST load an existing keypair or generate a new one on first use.

#### Scenario: First-time key generation
- Given no keypair files exist under `~/.podmesh/`
- When podctl runs any command requiring signing
- Then it generates a new Ed25519 signing keypair
- And writes `pubkey.bin` and `privkey.bin` to `~/.podmesh/` with mode 0o600

#### Scenario: Ephemeral key mode
- Given `--ephemeral` is set
- When podctl runs
- Then it generates in-memory keypairs and does not persist them to disk

### Requirement: podctl sends encrypted delete requests to each workload provider

The delete command MUST route encrypted `DeleteRequest` messages per-provider.

#### Scenario: Deleting a workload
- Given a manifest file is provided
- When `podctl delete` runs
- Then it computes the manifest_id from the spec file
- And queries `POST {server}/tasks/{manifest_id}/providers` to discover holding peers
- And for each provider, encrypts a `DeleteRequest` as an Envelope (ECIES + Ed25519 signed)
- And sends it to `DELETE {server}/delete_direct/{peer_id}`

### Requirement: podctl queries workload status without authentication

`get` and `logs` subcommands MUST issue plain HTTP requests with no auth.

#### Scenario: Listing running workloads
- Given the scheduler is reachable
- When `podctl get pods` runs
- Then it issues `GET {server}/runtime/workloads` with no auth headers
- And prints the returned workload list

### Requirement: podctl issues a tenant-signed NodeCert to a proxy node

podctl MUST provide a command that fetches a proxy node's public key material, constructs
a `NodeCert` signed with the owner's Ed25519 private key, and delivers it to the proxy.
This is a one-time provisioning step performed by the operator before any workload that
uses that proxy is deployed.

#### Scenario: Fetching proxy key material
- Given a proxy node is reachable at a known address
- When the operator runs `podctl grant-proxy --proxy-url <url>`
- Then podctl sends `GET <url>/api/v1/signing_pubkey` to retrieve the proxy's Ed25519 public key
- And sends `GET <url>/api/v1/kem_pubkey` to retrieve the proxy's X25519 public key
- And sends `GET <url>/api/v1/peer_id` (or derives it from the signing key) to retrieve the
  proxy's libp2p PeerId
- And aborts with an error if any of these requests fail or return malformed data

#### Scenario: Constructing and signing the proxy NodeCert
- Given the proxy's `peer_id`, `signing_pubkey`, and `kem_pubkey` have been retrieved
- And the operator's Ed25519 owner keypair is loaded from `~/.podmesh/`
- When podctl builds the cert
- Then it constructs a `NodeCert` with:
  - `peer_id` = proxy's libp2p PeerId
  - `signing_pubkey` = proxy's Ed25519 public key (base64)
  - `kem_pubkey` = proxy's X25519 public key (base64)
  - `capabilities` = `["proxy"]`
  - `role` = `NodeRole::Proxy`
  - `valid_until` = `now_unix_secs + ttl` (default TTL configurable, e.g. 90 days)
  - `owner_pubkey` = operator's Ed25519 public key (base64)
  - `owner_sig` = `Ed25519.sign(owner_sk, canonical_bytes(NodeCert))`
- And the cert MUST NOT be signed until all fields are populated (no partial signing)

#### Scenario: Delivering the signed cert to the proxy
- Given a signed `NodeCert` has been produced
- When podctl delivers it
- Then it POSTs the base64 postcard-encoded cert to `POST <proxy-url>/api/v1/node_cert`
- And the proxy responds with a success acknowledgement
- And podctl prints the `proxy_dht_key = hex(blake3(owner_pubkey_bytes)[..16])` that the
  proxy will announce under, so the operator can verify discovery

#### Scenario: Cert delivery is idempotent
- Given a proxy already holds a valid cert for this owner
- When podctl runs `grant-proxy` again for the same proxy
- Then the proxy replaces the stored cert if the new cert's `valid_until` is later
- And podctl does not error

## OBSERVED Trust Gaps

### Observation: Custodian list is trusted from the scheduler without independent verification

`GET /api/v1/custodians` is unauthenticated and its response is used directly to choose
who receives ECIES-wrapped DEK shares. A compromised or MITM'd scheduler could substitute
attacker-controlled KEM public keys, allowing share recovery without the intended custodians.

### Observation: manifest_id is 8 bytes (64 bits) truncated from blake3

Using only the first 8 bytes of the blake3 hash as the global workload identifier means that
collision resistance is limited to ~2^32 operations (birthday bound). In deployments with
many workloads this may become an issue.

### Observation: Status and log endpoints are fully unauthenticated

`get pods`, `get pod`, and `logs` issue plain HTTP GETs with no signature or token.
Anyone with network access to the scheduler REST API can enumerate running workloads and
retrieve their logs.
