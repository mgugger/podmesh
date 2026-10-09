## ADDED Requirements

### Requirement: Explicit HTTP proxy admission SHALL bound pre-tunnel work

When enabled, the local HTTP proxy SHALL enforce a 16 KiB combined request-line/header bound,
at most 64 headers, a five-second whole-head deadline from socket acceptance and at most 64
concurrent pre-tunnel handlers. Reads SHALL enforce bounds before unbounded allocation; per-line
progress SHALL NOT reset the overall deadline. These bounds do not imply public-network isolation.

Excess connections SHALL be closed before spawning work. Admitted handlers SHALL retain admission
until bounded tunnel handoff or failure; queue handoff and best-effort error writes SHALL each take
at most one second. Listener shutdown SHALL terminate outstanding pre-tunnel handlers, reclaim
permits and stop accepting work. Existing raw traffic/session bounds SHALL remain independently enforced.

#### Scenario: A request exceeds the head or header-count bound

- **WHEN** a client sends an excessive request line or header set
- **THEN** it is refused without unbounded buffering or enqueueing a tunnel
- **AND** any best-effort 431 response is time-bounded

#### Scenario: A client dribbles bytes or ends an incomplete head

- **WHEN** input cannot form a complete valid head within the total deadline, or EOF arrives early
- **THEN** the handler closes or returns a bounded error without creating a tunnel

#### Scenario: The listener or tunnel queue is saturated

- **WHEN** all pre-tunnel slots are occupied or queue handoff cannot complete within one second
- **THEN** the new request is refused and its socket/permit is released without an unbounded waiter

#### Scenario: The listener stops during partial input

- **WHEN** listener shutdown occurs while clients are reading headers or waiting to hand off
- **THEN** those handlers terminate and release their resources

### Requirement: HTTP admission hardening SHALL preserve authorized byte handoff

Bounded parsing SHALL preserve already-buffered HTTP body bytes and early CONNECT bytes exactly
once when handing the socket to the tunnel. The existing initial-data bound and authenticated
remote setup SHALL still apply; CONNECT success SHALL NOT be emitted before authorized setup.
Raw request lines, headers, URL userinfo and query data SHALL NOT be copied into diagnostics.

#### Scenario: Body bytes arrive in the same read as headers

- **WHEN** a valid head is followed by prefetched application bytes
- **THEN** the reconstructed origin request or CONNECT handoff forwards those bytes once and in order
- **AND** normal tunneled traffic keeps its existing authorization and stream limits