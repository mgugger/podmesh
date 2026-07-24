# Biscuit-Based Proxy And Sidecar Authorization

## ADDED Requirements

### Requirement: Sidecar registration is token-authorized

Registration MUST carry a tenant-issued token bound to workload ID and transport peer identity.

#### Scenario: Sidecar registers with a valid token
- **WHEN** a sidecar registration carries a valid tenant token bound to its workload and transport peer
- **THEN** the proxy accepts registration after NodeCert and signature verification

### Requirement: Ingress is token-authorized

Sidecars MUST reject ingress requests without a valid token permitting the requested workload and
path prefix.

#### Scenario: Ingress path is outside token scope
- **WHEN** a sidecar receives an ingress request whose path is not permitted by the token
- **THEN** it rejects the request without forwarding to the application

### Requirement: Egress is token-authorized

Proxies MUST reject tunnel requests without a valid token permitting the requested workload,
destination host, and destination port.

#### Scenario: Egress destination is outside token scope
- **WHEN** a sidecar requests a destination not permitted by its token
- **THEN** the proxy rejects the tunnel

### Requirement: Identity remains mandatory

Valid authorization MUST NOT compensate for invalid NodeCert or transport identity.

#### Scenario: Token is valid but identity is invalid
- **WHEN** token verification succeeds but NodeCert tenant or transport peer binding fails
- **THEN** the operation is rejected