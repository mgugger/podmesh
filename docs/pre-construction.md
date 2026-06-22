# PRE Construction for podmesh Key Release

## Overview

podmesh uses Proxy Re-Encryption (PRE) to allow custodian nodes to transform
a DEK (Data Encryption Key) ciphertext encrypted to the owner's key into a
ciphertext decryptable by an assigned worker node — without the custodian ever
learning the DEK.

The key release subsystem is the cryptographic backbone of podmesh's confidential
workload model. When an owner submits a manifest, the DEK that encrypts the workload
specification is never stored in the clear on any node. Instead, it is distributed
across custodian nodes in a form that requires multiple custodians to cooperate, and
that allows a specific authorized worker to reconstruct it without any single custodian
gaining access to the plaintext key material.

---

## Shamir Secret Sharing (custodian DEK shares)

In v1, the owner splits the DEK into N Shamir shares at apply time.
Each share is wrapped (encrypted) to a specific custodian's X25519 KEM pubkey.
Workers reconstruct the DEK by querying M-of-N custodians, each of which
wraps their share to the requesting worker's KEM pubkey before releasing it.

### Construction detail

At manifest submission time, the podmesh owner client:

1. Generates a random 256-bit DEK using a CSPRNG.
2. Encrypts the manifest with the DEK using XChaCha20-Poly1305.
3. Splits the DEK into N shares using Shamir's Secret Sharing over GF(2^8) or a
   prime field, with a reconstruction threshold of M.
4. For each custodian node i (i = 1..N), wraps share_i using
   `encrypt_payload_for_recipient(custodian_i_kem_pub, share_i)` — an ephemeral
   X25519 ECDH key agreement followed by XChaCha20-Poly1305 encryption.
5. Publishes the encrypted manifest, the per-custodian wrapped shares, and the
   assignment metadata to the scheduler.

At workload assignment time, the scheduler assigns a worker node and records a
signed assignment containing (manifest_id, worker_peer_id, assigned_at).

When the assigned worker needs to decrypt the manifest, it sends a `ShareRequest`
to each custodian. The custodian:

1. Verifies the worker's NodeCert and checks that `role != Custodian` (preventing
   custodian nodes from impersonating workers to pool shares).
2. Verifies the assignment signature from the scheduling custodian.
3. Checks a one-time-use table for the (manifest_id, worker_peer_id) pair to
   prevent replay attacks.
4. Decrypts its wrapped share using its own KEM private key.
5. Re-wraps the share to the worker's KEM pubkey using
   `encrypt_payload_for_recipient(worker_kem_pub, share)`.
6. Returns a `ShareResponse` containing the re-wrapped share and a custodian
   signature over (manifest_id, worker_peer_id, wrapped_share).

The worker collects M valid `ShareResponse` objects, verifies each custodian
signature, unwraps each share using its own KEM private key, and reconstructs
the DEK using Shamir reconstruction. It then decrypts the manifest.

### Security property

No single custodian can reconstruct the DEK. M-of-N custodians colluding can
reconstruct it, so M should be set to the majority threshold (e.g. M=2 of N=3).
This means an adversary must compromise a strict majority of custodian nodes to
obtain the DEK.

### Pooling prevention

Share release is bound to the requesting worker's KEM pubkey and is one-time per
(manifest_id, worker_peer_id) pair. Custodians with role=Custodian in their NodeCert
are rejected as share requesters — a custodian cannot pretend to be a worker in order
to collect enough shares to reconstruct the DEK itself.
