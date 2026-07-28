# Message Security Specification

## Purpose

Every workload-bearing and lifecycle message in Podmesh is signed and encrypted so that no
non-selected node is trusted with tenant data. This capability defines the envelope, the primitives,
and the validation rules that all components share.

## Requirements

### Requirement: Workload and lifecycle messages SHALL be signed and encrypted

Complete workload specifications and lifecycle commands SHALL be signed by the namespace owner and
encrypted to the selected agent's KEM key. Only the owner and the selected agent SHALL be able to
read them.

#### Scenario: Intermediary sees only ciphertext

- **WHEN** a scheduler relays a workload payload
- **THEN** it observes only opaque bytes and cannot recover the specification

### Requirement: Every message SHALL travel in a validated envelope

Messages SHALL be wrapped in an `Envelope` carrying the payload, payload type, nonce, millisecond
timestamp, algorithm, signature, sender signing public key, peer identifier, and sender KEM public
key. Validation SHALL use the shared `EnvelopeValidator`.

#### Scenario: Replayed nonce is refused

- **WHEN** an envelope reuses a nonce already recorded by the receiver
- **THEN** the envelope is refused

#### Scenario: Timestamp outside the drift window is refused

- **WHEN** an envelope's timestamp falls outside the accepted clock drift window
- **THEN** the envelope is refused

#### Scenario: Unsigned message in strict mode is refused

- **WHEN** an unsigned message arrives while strict mode is enabled
- **THEN** it is refused

### Requirement: Capacity messages SHALL be signed, bounded, and short-lived

`CapacityQuery` and `CapacityOffer` SHALL be signed, size-bounded, short-lived, and
replay-resistant. They need not be encrypted, because they carry no workload or tenant data.

#### Scenario: Oversized capacity message is dropped

- **WHEN** a gossiped capacity message exceeds its size bound
- **THEN** it is dropped without further parsing

#### Scenario: Stale capacity offer is ignored

- **WHEN** an offer's validity window has elapsed
- **THEN** the scheduler ignores it during selection

### Requirement: Cryptographic primitives SHALL be fixed

Signatures SHALL use Ed25519, key exchange SHALL use X25519, and symmetric encryption SHALL use
XChaCha20-Poly1305. Delegatable service grants SHALL use Biscuit tokens.

#### Scenario: Unsupported algorithm is refused

- **WHEN** an envelope declares an algorithm other than the supported signature algorithm
- **THEN** it is refused

### Requirement: Keys SHALL be stored with restrictive permissions

Persisted key material SHALL be written with `0600` permissions. Client keys live under
`~/.podmesh/`; agent keys live under the configured agent key directory. Ephemeral in-memory keys
SHALL be available for testing only.

#### Scenario: Key files are not world readable

- **WHEN** a component creates its key files
- **THEN** the files are readable only by their owner
