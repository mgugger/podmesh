# Sidecar–Proxy Auth Tasks

- [x] Add `Proxy` variant to `NodeRole` enum in `shared/protocol/src/node_cert.rs`
- [x] Extend `SidecarRegistration` with `sidecar_signing_pubkey: String` in `shared/protocol/src/sidecar_registration.rs`
- [x] Extend handshake response envelope to carry optional `proxy_cert_b64` field in `shared/p2p/src/handshake.rs`
- [x] Implement obfuscated DHT key derivation: `hex(blake3(owner_pubkey_bytes)[..16])` in `shared/protocol/src/tenant_dht.rs`
- [x] Add proxy announcement under derived tenant DHT key in `podmesh-proxy/src/p2p.rs`
- [x] Add `podctl grant-proxy` command: fetch proxy key material, sign NodeCert with owner keypair, POST to proxy `/api/v1/node_cert`
- [x] Add `POST /api/v1/node_cert` REST endpoint to proxy: verify cert fields (peer_id match, role, expiry, sig), store durably keyed by owner_pubkey
- [x] Implement proxy cert verification in sidecar after handshake (owner_sig, owner_pubkey match, expiry, peer_id bind)
- [x] Implement SidecarRegistration verification in proxy (sidecar sig, owner_pubkey match, transport peer_id bind)
- [x] Remove sidecar fallback to unauthenticated `podmesh-proxy-node` DHT key for workload traffic
- [x] Write unit tests: cert sign/verify, obfuscated key derivation determinism, mismatched owner rejected, expired cert rejected, wrong transport peer_id rejected, cross-tenant registration rejected

