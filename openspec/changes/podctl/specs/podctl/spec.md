# podctl

## ADDED Requirements

### Requirement: Complete workload is encrypted by default

`podctl` MUST encrypt the full normalized execution specification with XChaCha20-Poly1305 and MUST
wrap the random DEK only to the selected agent's X25519 key.

#### Scenario: Apply encrypts the normalized workload
- **WHEN** the namespace owner applies a valid workload manifest
- **THEN** podctl encrypts the complete normalized execution specification with a fresh DEK
- **AND** wraps the DEK only to the selected agent KEM key

### Requirement: Owner signs complete deployment grant

The Ed25519 namespace key MUST sign namespace, full workload/revision IDs, target agent,
reservation, encrypted capsule, wrapped DEK, nonce, and expiry.

#### Scenario: Deployment grant is owner-bound
- **WHEN** podctl builds a deployment grant
- **THEN** the namespace key signs every target, identity, reservation, ciphertext, nonce, and expiry field

### Requirement: Lifecycle commands are direct and encrypted

Status, logs, and deletion MUST be owner-signed, replay-bounded, and encrypted directly to the agent.

#### Scenario: Owner requests workload status
- **WHEN** podctl requests status using a stored receipt
- **THEN** it sends an owner-signed, expiring, nonce-bound command encrypted to the receipt agent

### Requirement: Receipt catalog is local and non-authoritative

`podctl` MUST persist verified deployment receipts with mode `0600`. Loss of the local catalog MUST
NOT stop a running workload, though it may prevent management until the receipt is restored.

#### Scenario: Receipt catalog is lost
- **WHEN** the local receipt catalog is unavailable
- **THEN** already running workloads continue without scheduler or client state