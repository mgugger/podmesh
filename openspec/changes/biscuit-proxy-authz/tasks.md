# Biscuit Mesh Authz Tasks (Proxy/Sidecar + Custodian Delegation)

- [ ] Add `biscuit-auth` dependency in relevant crates (`shared/protocol`, `podmesh-proxy`, `podmesh-sidecar`, `podmesh-scheduler`, optionally `podctl`)
- [ ] Implement schema v1 from `specs/biscuit-token-schema/spec.md` in `shared/` (required facts, ambient context, deterministic checks)
- [ ] Add canonical verifier API in `shared/` (single decision function used by proxy + sidecar)
- [ ] Extend `SidecarRegistration` with `authz_token_b64` and add serialization tests
- [ ] Extend ingress proxy request type with token field and add protocol compatibility tests
- [ ] Extend egress tunnel request type with token field and add protocol compatibility tests
- [ ] Extend custodian `ShareRequest` (and delegated handoff message, if separate) with `authz_token_b64`
- [ ] Implement proxy-side Biscuit verification for sidecar registration
- [ ] Implement sidecar-side Biscuit verification for inbound ingress requests
- [ ] Implement proxy-side Biscuit verification for egress tunnel requests and destination caveats
- [ ] Implement custodian-side Biscuit verification for `release_share`
- [ ] Implement attenuated custodian-to-custodian delegation flow (`delegate_share`) with delegate binding, depth checks, and expiry narrowing
- [ ] Implement token minting path in `podctl` (or scheduler-issued attenuation path) for proxy delegation
- [ ] Add unit tests for schema conformance: missing required facts rejected, wrong operation rejected, wrong tenant root key rejected, delegation depth exhaustion rejected
- [ ] Add end-to-end tests: valid token accepted, expired token rejected, wrong manifest rejected, wrong peer rejected, ingress path violation rejected, egress destination violation rejected, unauthorized custodian delegate rejected
- [ ] Update docs for operational flow, key rotation, and migration plan
