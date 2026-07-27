## Why

Kademlia discovery couples the workload plane to libp2p and does not match the planned Iroh
transport, while ephemeral proxy identities make long-lived workloads unable to reconnect after a
proxy process is replaced. Global compute also needs tenant-owned, stable proxy entry points that
can refer sidecars to regional peers without putting tenant state in the scheduler.

## What Changes

- **BREAKING** Require every proxy to load a durable libp2p identity from protected persistent
  storage; initialized proxies fail instead of silently changing identity.
- **BREAKING** Replace sidecar DNS/DHT bootstrap metadata with a bounded list of known proxy
  multiaddrs containing stable peer IDs.
- Add a bounded request-response protocol through which a sidecar asks a connected, verified proxy
  for additional proxy addresses for the same tenant.
- Require every returned proxy candidate to be verified through the existing tenant-issued
  `NodeCert` handshake before registration or workload traffic.
- Remove Kademlia behaviours, provider announcements, provider lookups, mutable records, and the
  proxy manifest-provider fallback from the proxy and sidecar.
- Keep authenticated sidecar registration as the sole proxy routing source; unregistered workloads
  are unavailable at that proxy until the sidecar registers.
- Preserve regional failover by allowing each workload to start with multiple tenant proxy IDs and
  learn further regional proxies after connecting.

## Capabilities

### New Capabilities

- `tenant-proxy-discovery`: Durable proxy identity, explicit sidecar bootstrap peers, authenticated
  peer exchange, regional candidate discovery, and Kademlia-free routing requirements.

### Modified Capabilities

None.

## Impact

- Affects `shared/p2p`, `shared/protocol`, `podmesh-proxy`, `podmesh-sidecar`, `podmesh-agent`,
  deployment configuration, and integration tests.
- Changes `SidecarMetadata` and proxy CLI configuration without backward compatibility.
- Removes the libp2p Kademlia dependency from runtime behaviours; libp2p remains the current
  transport until the later Iroh migration.
- Requires durable secret storage for each logical proxy and fencing to prevent concurrent use of
  one proxy identity by multiple active instances.
- Does not add scheduler state or a new server type.