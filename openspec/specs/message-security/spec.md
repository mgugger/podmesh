# Message Security Specification

## Purpose

Every workload-bearing and lifecycle message in Podmesh is signed and encrypted so that no
non-selected node is trusted with tenant data. This capability defines the envelope, the sealed-box
construction, the signing rules, and the freshness rules that all components share.

## Requirements

### Requirement: Workload and lifecycle messages SHALL be signed and encrypted

Complete workload specifications and lifecycle commands SHALL be signed by the namespace owner and
encrypted to the selected agent's KEM key. Only the owner and the selected agent SHALL be able to
read them.

#### Scenario: Intermediary sees only ciphertext

- **WHEN** a scheduler relays a workload payload
- **THEN** it observes only opaque bytes and cannot recover the specification

#### Scenario: Payload sealed to another agent is unusable

- **WHEN** an agent receives a payload sealed to a different agent's KEM key
- **THEN** decryption fails and the message is refused

### Requirement: The sealed-box construction SHALL bind key material to its recipient

The AEAD key SHALL be derived, under a fixed context string, from the Diffie-Hellman output together
with the ephemeral public key and the recipient public key. The raw Diffie-Hellman output SHALL NOT
be used as a key. A shared secret that is not contributory SHALL be refused, so a low-order recipient
key cannot force a predictable key. The blob header — version, ephemeral public key, nonce, and
declared ciphertext length — SHALL be authenticated as associated data, and a blob SHALL have exactly
one valid encoding.

#### Scenario: Low-order recipient key is refused

- **WHEN** a message is sealed to an X25519 key that drives the shared secret to zero
- **THEN** sealing fails rather than producing a key the sender's peers can predict

#### Scenario: Rewritten header is refused

- **WHEN** any byte of a sealed blob's header is altered
- **THEN** decryption fails

#### Scenario: Trailing bytes are refused

- **WHEN** a sealed blob carries bytes beyond its declared ciphertext length
- **THEN** decryption fails rather than ignoring them

### Requirement: Every signature SHALL be bound to its message type

Each signed message type SHALL have its own domain label, and that label SHALL be bound into the
bytes Ed25519 signs. Signature verification SHALL reject non-canonical signatures and public keys of
small order.

#### Scenario: A signature does not transfer between message types

- **GIVEN** two message types whose canonical encodings coincide
- **WHEN** a signature produced for one is presented for the other
- **THEN** verification fails

### Requirement: Owner-signed agent messages SHALL name their target agent

An admission request, deployment grant, and lifecycle command SHALL each name the agent it is
addressed to. An agent SHALL refuse a message naming a different agent.

#### Scenario: One signed request cannot be fanned out across the mesh

- **WHEN** a relay presents an owner-signed admission request to an agent it does not name
- **THEN** that agent refuses it and reserves nothing

### Requirement: Owner-signed agent messages SHALL have a bounded lifetime

Each such message SHALL carry an issue time and an expiry. The validity window SHALL NOT exceed
`MAX_AGENT_MESSAGE_LIFETIME_SECS`, and SHALL be evaluated against a bounded clock-skew allowance. A
message with an unbounded or inverted window SHALL be refused.

#### Scenario: An unbounded validity window is refused

- **WHEN** a message declares an expiry far beyond the maximum lifetime
- **THEN** it is refused regardless of its signature

### Requirement: Every peer-to-peer message SHALL travel in a validated envelope

Messages on the workload plane SHALL be wrapped in an `Envelope` carrying the version, payload,
payload type, nonce, millisecond timestamp, algorithm, sender endpoint id, **recipient endpoint id**,
sender signing key, sender KEM key, and signature. Acceptance SHALL happen only through the shared
`EnvelopeValidator`.

There SHALL be no permissive mode. An unsigned envelope SHALL always be refused.

#### Scenario: Unsigned envelope is refused

- **WHEN** an envelope arrives without a signature
- **THEN** it is refused

#### Scenario: Envelope addressed to another endpoint is refused

- **WHEN** a relay forwards a valid envelope to an endpoint it does not name as recipient
- **THEN** that endpoint refuses it

#### Scenario: Envelope claiming another sender is refused

- **WHEN** an envelope's sender does not match the authenticated transport peer
- **THEN** it is refused

#### Scenario: A response cannot be replayed as a request

- **WHEN** a handshake response is presented where a request is expected
- **THEN** it is refused, because the direction is part of the signed payload type

#### Scenario: Replayed nonce is refused

- **WHEN** an envelope reuses a nonce already recorded for that peer
- **THEN** the envelope is refused

#### Scenario: Timestamp outside the drift window is refused

- **WHEN** an envelope's timestamp falls outside the accepted clock drift window
- **THEN** the envelope is refused

### Requirement: Replay caches SHALL be bounded and SHALL evict rather than fail closed

A replay cache SHALL bound both the number of tracked peers and the entries per peer, and SHALL
evict its oldest entries when full. It SHALL NOT begin refusing new messages on reaching capacity,
because that would let one sender deny service to every other.

#### Scenario: A flood does not deny service to other senders

- **WHEN** one peer submits more messages than its share of the cache
- **THEN** further messages from that peer and from other peers are still accepted

### Requirement: Capacity messages SHALL be signed, bounded, and short-lived

`CapacityQuery` and `CapacityOffer` SHALL be signed, size-bounded, short-lived, and replay-resistant.
They need not be encrypted, because they carry no workload or tenant data.

#### Scenario: Oversized capacity message is dropped

- **WHEN** a gossiped capacity message exceeds its size bound
- **THEN** it is dropped without further parsing

#### Scenario: Stale capacity offer is ignored

- **WHEN** an offer's validity window has elapsed
- **THEN** the scheduler ignores it during selection

### Requirement: Cryptographic primitives SHALL be fixed

Signatures SHALL use Ed25519, key exchange SHALL use X25519, key derivation SHALL use BLAKE3, and
symmetric encryption SHALL use XChaCha20-Poly1305. Delegatable service grants SHALL use Biscuit
tokens.

#### Scenario: Unsupported algorithm is refused

- **WHEN** an envelope declares an algorithm other than the supported signature algorithm
- **THEN** it is refused

### Requirement: Each component SHALL load its identity from an explicit directory

Key loading SHALL take the key directory as a parameter rather than reading process-wide state, so
that two components in one process each keep a distinct identity.

#### Scenario: Two components in one process do not share an identity

- **WHEN** two components are started with different key directories
- **THEN** their signing and KEM keys differ

### Requirement: Keys SHALL be written atomically with restrictive permissions

Persisted key material SHALL be created with `0600` permissions in a single operation, never widened
after the fact. A private key file whose permissions were later widened SHALL be refused on load, and
a public key that does not match the private key beside it SHALL be refused. Client keys live under
`~/.podmesh/` unless overridden; agent keys live under the configured agent key directory.

#### Scenario: A key file is never briefly world readable

- **WHEN** a component creates its key files
- **THEN** the file is created with owner-only permissions rather than narrowed afterwards

#### Scenario: A widened key file is refused

- **WHEN** a private key file's permissions have been widened since it was written
- **THEN** loading it fails

#### Scenario: A mismatched keypair is refused

- **WHEN** the public and private key files in a directory do not form a keypair
- **THEN** loading them fails
