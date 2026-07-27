## Context

The workload plane currently creates a fresh libp2p identity on every proxy and sidecar start.
Sidecars receive a DNS bootstrap multiaddr, join Kademlia, discover tenant proxy providers under a
tenant-derived key, and publish manifest provider records. Proxies first route through authenticated
sidecar registrations but fall back to Kademlia manifest-provider lookup.

The target system is a global compute mesh with tenant-operated regional proxies. A workload may
outlive a proxy process or host, so a logical proxy identity must survive ordinary replacement.
The scheduler must remain stateless and unaware of tenant discovery, and the discovery contract
must remain usable when libp2p is later replaced by Iroh.

## Goals / Non-Goals

**Goals:**

- Persist one stable cryptographic identity for each logical proxy.
- Bootstrap sidecars from several explicit proxy identities and dialable addresses.
- Let an authenticated sidecar fetch additional regional proxy candidates from any connected proxy.
- Remove all Kademlia behaviours, records, provider operations, and query state.
- Keep discovery results untrusted until the existing tenant `NodeCert` handshake succeeds.
- Keep peer caches and route registrations bounded, expiring, and reconstructible.

**Non-Goals:**

- Implement the Iroh transport migration.
- Introduce DNS, a rendezvous executable, durable membership state, or scheduler-owned discovery.
- Support active-active processes sharing one proxy private key.
- Recover when every initial proxy identity and its private-key backup are lost.
- Preserve compatibility with existing `SidecarMetadata` or Kademlia-based deployments.

## Decisions

### Persist the libp2p identity separately from application keys

`podmesh-proxy` will load a protobuf-encoded libp2p Ed25519 keypair from its configured key
directory. On first initialization it creates the file atomically with mode `0600`; subsequent
starts fail if the file is absent or malformed rather than silently changing `PeerId`.

The transport identity remains separate from the existing application signing and X25519 KEM keys
because the current `NodeCert` binds all three identities explicitly. The later Iroh implementation
can replace the transport-key loader while preserving the same durable-identity lifecycle.

Alternative considered: derive the transport identity from the application signing key. Rejected
because it couples key formats and expands the blast radius of one key compromise.

### Bootstrap records contain identity and addresses

`SidecarMetadata` will contain a bounded list of `ProxyPeer` records. Each record contains a
libp2p `PeerId` and bounded dialable multiaddrs. Every address must end in the same `/p2p/<peer-id>`;
a bare peer ID cannot be dialed without an external address-resolution service.

The tenant supplies these records through `podmesh.io/proxy-peers` or the local `podctl`
environment. `podctl` validates and embeds them in the encrypted, owner-signed `ExecutionSpec`, and
the agent copies them into sidecar metadata after decryption. Sidecars race or retry several
candidates, not one DNS alias.

Alternative considered: store only peer IDs. Rejected because removing Kademlia and DNS leaves no
way to resolve a libp2p peer ID to an address.

### Add bounded tenant-scoped proxy peer exchange

A request-response protocol `/podmesh/proxy-discovery/1.0.0` will support a sidecar request carrying
its tenant owner public key and a result limit. A proxy returns a bounded list of known proxy peer
records excluding the requester and local duplicates.

Returned records are candidates, not authorization. The sidecar connects and accepts a proxy only
after the existing handshake verifies a non-expired tenant-issued `NodeCert`, role `Proxy`, and
transport-peer binding. Invalid candidates are discarded and never used for registration or egress.

Initially a proxy's known set is built from configured proxy peers and connected proxy peers with
known addresses. The set is bounded and in memory. This is sufficient for regional meshes where
proxies maintain several inter-region connections; later gossip can distribute the same records
without changing the sidecar protocol.

### Direct registration is the only workload routing source

The proxy will no longer query manifest providers. A sidecar registers its routes with every
verified proxy it uses, and periodically refreshes that registration. If a proxy has no valid route
registration for a workload, it returns service unavailable.

This makes loss of discovery state non-authoritative: running workloads are unaffected, and route
tables rebuild when sidecars reconnect and register.

### Remove Kademlia completely from proxy and sidecar behaviours

The Kademlia behaviour, configuration constants, provider records, manifest publication, query
state, readiness signals, and compatibility helpers will be deleted. Gossipsub remains for the
current proxy mesh but does not establish tenant authorization.

## Risks / Trade-offs

- **All bootstrap peers are unreachable** -> Provision at least two regional proxy records per
  workload and let connected proxies return additional candidates.
- **A returned address is malicious or stale** -> Bound connection attempts and require tenant
  `NodeCert` verification before adding the peer to the eligible set.
- **One private key runs concurrently in two regions** -> Document single-writer fencing and use a
  distinct durable identity for each active regional proxy.
- **Proxy key loss strands old bootstrap records** -> Require protected backups and multiple
  independently keyed initial proxies; later DNS or owner-signed updates can provide disaster
  recovery.
- **Removing manifest fallback reduces availability during reconnect** -> Sidecars register with
  multiple verified proxies and refresh registration; proxies fail closed without a route.
- **Peer exchange leaks mesh candidates** -> Require a verified sidecar connection, tenant-scope the
  request, cap results, and reveal no workload data.

## Migration Plan

1. Add persistent proxy identity configuration and initialize each deployed proxy key directory.
2. Issue new tenant `NodeCert`s bound to the durable peer IDs.
3. Configure agents with multiple address-bearing proxy peer records.
4. Deploy sidecars and proxies supporting peer exchange and direct registration only.
5. Remove Kademlia configuration and old deployments after all workloads use the new metadata.

Rollback requires restoring the previous binaries and metadata together; metadata compatibility is
intentionally not provided.

## Open Questions

- The later Iroh change must choose whether the peer record carries only `EndpointId` and relies on
  Iroh address lookup, or also carries current `EndpointAddr` hints.
- Disaster recovery after loss of every tenant proxy identity still requires an owner-controlled
  external update mechanism such as DNS or a signed bootstrap record.