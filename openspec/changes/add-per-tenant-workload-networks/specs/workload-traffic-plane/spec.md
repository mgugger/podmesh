## ADDED Requirements

### Requirement: Peer seeds SHALL preserve the local execution catalog

A sidecar SHALL refuse a peer seed whose catalog names its own manifest but differs from its
configured execution catalog, without changing cache or hint state. Owner-mediated seed delivery
may select remote catalogs, but changing the local execution catalog requires deployment update.

#### Scenario: A rolling update introduces an older sibling

- **GIVEN** a replacement sidecar configured with the new owner-signed catalog
- **WHEN** seed control introduces a sibling advertising the previous catalog
- **THEN** the seed is refused and the replacement retains its own catalog and lease-refresh authority
- **AND** a matching-catalog seed remains admissible

### Requirement: Peer setup failures SHALL remain stream-scoped

A failed or timed-out peer synchronization/service setup SHALL NOT close an otherwise healthy
shared transport or reset independent authorized service streams. Every operation SHALL retain
its signed-envelope, credential, replay, size, concurrency and deadline checks.

#### Scenario: A failed synchronization shares a live service connection

- **GIVEN** an established authorized service tunnel over a cached peer transport
- **WHEN** a concurrent synchronization fails catalog validation
- **THEN** synchronization fails without terminating the established tunnel
- **AND** application bytes continue to flow under the tunnel's existing authority checks
