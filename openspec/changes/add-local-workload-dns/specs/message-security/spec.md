## ADDED Requirements

### Requirement: Delegated identity issuance SHALL be explicitly constrained
An owner-rooted delegation SHALL bind its issuer key, tenant, workload, replica/incarnation policy and permitted operations. Child identity authorization SHALL be verified against that delegation with scope intersection and a bounded chain depth. Holder-added facts SHALL NOT establish issuer authority.

#### Scenario: Issuer widens a child scope
- **WHEN** a correctly signed child names another service, replica or unauthorized operation
- **THEN** the verifier refuses it

#### Scenario: An unapproved key issues a credential
- **WHEN** a child signature does not match the delegated issuer key
- **THEN** the verifier refuses it

### Requirement: Identity authorization SHALL bind the keys actually used
Workload authorization SHALL bind the authenticated Iroh EndpointId and the application signing key used for detached service records and workload envelopes. Any additional key used as an authorization identity SHALL have an explicit verified binding.

#### Scenario: Detached record substitutes its signer
- **WHEN** a service record verifies under a key not authorized for its endpoint
- **THEN** the record is refused

### Requirement: Child renewal SHALL preserve parent authority
Automatic child renewal SHALL require proof of authorized identity possession and SHALL remain within parent scope and validity. An expired parent SHALL NOT be extended by issuing another child. Enrollment SHALL correlate fresh proof with the intended replica and reject unauthorized incarnation changes.

#### Scenario: Renewal proceeds without the CLI
- **WHEN** an authorized sidecar renews under valid delegated authority
- **THEN** no owner CLI connection or private owner key is required

#### Scenario: Parent expires
- **WHEN** renewal would extend child validity past finite parent expiry
- **THEN** the child is capped or issuance is refused

#### Scenario: Copied enrollment request is reused for another replica
- **WHEN** a request has the wrong replica, nonce or incarnation
- **THEN** enrollment is refused

### Requirement: Peer discovery and tunnel setup SHALL use bounded typed envelopes
Peer handshake, record synchronization and internal tunnel setup SHALL use operation/direction-specific validated envelopes with authenticated sender/recipient binding, freshness, deadlines, replay handling and size bounds. Raw traffic SHALL start only after authorized setup.

#### Scenario: Proxy-only operation arrives on a peer session
- **WHEN** a peer attempts an operation outside its session role
- **THEN** the operation is refused

#### Scenario: A request is replayed or redirected
- **WHEN** its nonce is retained or its sender/recipient does not match the connection
- **THEN** validation refuses the stream before changing discovery state or forwarding bytes
