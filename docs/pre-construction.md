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

## v1: Shamir Secret Sharing (current)

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

---

## v2: Umbral PRE (planned)

### Overview

In v2, podmesh will replace Shamir secret sharing with an Umbral-style proxy
re-encryption scheme. The key conceptual shift is that custodians will hold
*re-encryption key fragments* (kfrags) rather than DEK shares. A worker can ask
M-of-N custodians to each apply their kfrag to a *capsule* (a compact ciphertext
component produced during encryption) to generate *capsule fragments* (cfrags).
The worker combines M cfrags to recover the symmetric key without any custodian
ever decrypting it.

### Umbral construction

Umbral PRE is based on threshold proxy re-encryption as described by Nuñez, Agudo,
and Lopez (NuCypher, 2017). The scheme works over an elliptic curve group (typically
secp256k1 or Curve25519 in its prime-order form).

**Key generation:**
- The owner (delegator) has a keypair (sk_A, pk_A).
- The worker (delegatee) has a keypair (sk_B, pk_B).
- Each custodian has a keypair (sk_C_i, pk_C_i) used for key fragment distribution.

**Capsule generation (at encrypt time):**
When the owner encrypts the DEK, the Umbral library produces a *capsule* C = (E, V, s)
where E and V are elliptic curve points derived from a fresh ephemeral scalar, and s
is a scalar checksum. The DEK is derived from a hash of a key agreement involving the
owner's private key and the ephemeral point. The capsule is stored alongside the
ciphertext and is public — it reveals nothing about the DEK.

**Re-encryption key generation:**
The owner generates M-of-N re-encryption key fragments (kfrags) using a verifiable
secret sharing scheme. Each kfrag encodes a transformation from pk_A to pk_B, split
across N custodians with threshold M. The kfrag for custodian i is:
  kfrag_i = rk + share_i
where rk is the re-encryption key (derived from sk_A and pk_B) and share_i is a
Shamir share of a blinding factor. Kfrags are delivered to custodians over an
encrypted channel (wrapped to each custodian's pubkey) and are themselves
non-reconstructible into rk without the owner's private key.

**Re-encryption (at share release time):**
A custodian holding kfrag_i applies it to the capsule C to produce a capsule
fragment cfrag_i = kfrag_i * E (elliptic curve scalar multiplication on the
capsule's ephemeral point). The cfrag is compact (one curve point) and reveals
nothing about sk_A or sk_B.

**Decryption (by worker):**
The worker collects M cfrags and reconstructs the re-encrypted capsule
C' = combine(cfrag_1, ..., cfrag_M). It then derives the DEK using its own
private key sk_B and C', by computing the same key derivation function that the
owner used but now anchored to sk_B instead of sk_A. No individual custodian
ever holds the full capsule or the DEK.

### Security properties — custodian blindness proof sketch

The security of Umbral rests on the Decisional Diffie-Hellman (DDH) assumption
over the chosen elliptic curve group.

**Custodian blindness:** A custodian holding kfrag_i can compute cfrag_i = kfrag_i * E
for a given capsule E but cannot:
- Reconstruct the DEK without learning sk_B (cfrag_i alone is a single group element
  with no information about the symmetric key).
- Reconstruct the re-encryption key rk without colluding with M-1 other custodians
  (by the security of the underlying Shamir scheme over the field).

**Collusion bound:** M-1 custodians colluding cannot reconstruct rk or the DEK.
With M colluding custodians, they can produce a valid re-encrypted capsule, but only
for the designated delegatee worker (pk_B). They cannot use this to decrypt to any
other key without knowing sk_B.

**Non-transitivity:** Re-encryption cannot be chained — a custodian cannot take a
cfrag for worker B and produce a cfrag for worker C without a new kfrag from the owner.

### Key generation flow

1. Owner submits manifest. podmesh client generates (sk_A, pk_A) or reuses the owner's
   existing identity keypair.
2. Owner encrypts DEK under pk_A using Umbral, producing capsule C and ciphertext.
3. Owner generates kfrags for the assigned worker's pk_B:
   ```
   kfrags = umbral::generate_kfrags(sk_A, pk_B, threshold=M, shares=N)
   ```
4. Each kfrag_i is wrapped to custodian_i's pubkey using ECIES and stored on the
   scheduler alongside the capsule.

### Re-encryption flow

When a worker requests key material:

1. Worker sends a `ShareRequest` (with assignment proof and worker_kem_pub = pk_B)
   to each custodian.
2. Each custodian i unwraps kfrag_i, verifies the assignment, and computes:
   ```
   cfrag_i = umbral::reencrypt(kfrag_i, capsule)
   ```
3. Custodian returns cfrag_i (with a signature) as the `wrapped_share` field of
   `ShareResponse`. In v2, `wrapped_share` encodes a serialized cfrag rather than a
   symmetric-key-wrapped DEK share.

### Decryption by worker

1. Worker collects M `ShareResponse` objects and verifies custodian signatures.
2. Worker reconstructs the plaintext DEK:
   ```
   dek = umbral::decrypt_reencrypted(sk_B, pk_A, capsule, [cfrag_1, ..., cfrag_M], ciphertext)
   ```
3. Worker uses the DEK to decrypt the manifest with XChaCha20-Poly1305.

### v1→v2 migration path

Manifests include a `submission_version` field (via the `podmesh.io/submission-version`
annotation and the `ShareRequest.node_cert_bytes` chain). A v1 custodian stores wrapped
DEK shares. A v2 custodian stores kfrags and cfrags.

Migration steps:
1. Introduce the `submission_version` field (done in Phase 3).
2. Custodian nodes advertise supported versions in their NodeCert capabilities.
3. The scheduler, at apply time, selects the highest mutually supported version
   between the owner client and all assigned custodians.
4. Old v1 manifests continue to be served by v1 custodians; new submissions use v2
   when all assigned custodians support it.
5. An owner may re-submit a manifest to upgrade it from v1 to v2 by re-encrypting
   the DEK under the new Umbral scheme and distributing new kfrags.

### Why simple X25519 ECIES cannot support re-encryption without owner privkey

In standard X25519 ECIES, the ciphertext is:
  `(ephemeral_pub, AEAD(shared_secret, plaintext))`
where `shared_secret = ECDH(ephemeral_priv, recipient_pub)`.

To re-encrypt this for a different recipient, a proxy would need to:
1. Compute the original `shared_secret` — requiring either the owner's private key or
   the ephemeral private key (which is discarded after encryption).
2. Derive a new `shared_secret'` for the new recipient and re-wrap the ciphertext.

Neither is possible without the owner's involvement, because the ECDH shared secret is
ephemeral and the owner's static private key is required to re-derive it from the
ciphertext alone. Umbral solves this by splitting the key derivation across a
verifiable secret sharing step at encryption time, allowing custodians to perform
partial transformations on the *capsule* (a public ciphertext component) using only
their kfrag, without any of them holding the plaintext key or the owner's private key.
