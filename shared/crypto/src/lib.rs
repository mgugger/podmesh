use base64::{Engine as _, engine::general_purpose};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::RngCore;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey, StaticSecret};
use zeroize::Zeroizing;

use dirs::home_dir;

pub mod domain;
pub mod logging;
pub mod nonce_helper;

pub use domain::SignatureDomain;

pub const KEY_DIR: &str = ".podmesh";
pub const PUBKEY_FILE: &str = "pubkey.bin";
pub const PRIVKEY_FILE: &str = "privkey.bin";
pub const KEM_PUBFILE: &str = "kem_pub.bin";
pub const KEM_PRIVFILE: &str = "kem_priv.bin";

/// Mode bits every persisted private key file must carry.
pub const SECRET_FILE_MODE: u32 = 0o600;
/// Mode bits the key directory must carry.
pub const KEY_DIR_MODE: u32 = 0o700;

/// Key-derivation context for the sealed-to-recipient blob. `blake3::derive_key`
/// binds this string into the derived key, so a shared secret produced for one
/// purpose can never be reused as a key for another.
const KEM_KDF_CONTEXT: &str = "podmesh 2026-01-01 sealed-box v4 xchacha20poly1305 key";

/// Version byte of the sealed-to-recipient blob. Bumped from `0x03` when the raw
/// Diffie-Hellman output stopped being used directly as the AEAD key.
const SEALED_BLOB_VERSION: u8 = 0x04;

/// Fixed byte offsets inside a sealed blob:
/// `[version 1][ephemeral_pub 32][nonce 24][ctlen 4][ciphertext ctlen]`.
const SEALED_HEADER_LEN: usize = 1 + X25519_PUBLIC_KEY_SIZE + XCHACHA20_NONCE_SIZE + 4;

/// ed25519 key sizes
pub const ED25519_PUBLIC_KEY_SIZE: usize = 32;
pub const ED25519_PRIVATE_KEY_SIZE: usize = 32;
pub const ED25519_SIGNATURE_SIZE: usize = 64;

/// X25519 key sizes  
pub const X25519_PUBLIC_KEY_SIZE: usize = 32;
pub const X25519_PRIVATE_KEY_SIZE: usize = 32;

/// XChaCha20-Poly1305 nonce size
pub const XCHACHA20_NONCE_SIZE: usize = 24;

/// Poly1305 authentication tag size, appended to every ciphertext.
pub const POLY1305_TAG_SIZE: usize = 16;

/// Encode bytes to base64 string using STANDARD encoding.
#[inline]
pub fn b64_encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

/// Decode base64 string to bytes using STANDARD encoding.
#[inline]
pub fn b64_decode(data: &str) -> anyhow::Result<Vec<u8>> {
    general_purpose::STANDARD
        .decode(data)
        .map_err(|e| anyhow::anyhow!("base64 decode error: {}", e))
}

/// Generate a cryptographically secure random nonce as a hex string.
pub fn generate_secure_nonce() -> String {
    let mut nonce_bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    nonce_bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ensure_key_dir(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| anyhow::anyhow!("inspect key directory {}: {e}", path.display()))?;
        anyhow::ensure!(
            metadata.is_dir(),
            "key path {} is not a directory",
            path.display()
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(KEY_DIR_MODE))?;
    }
    Ok(())
}

/// Write a secret to `path` so it is never observable with wider permissions
/// than intended.
///
/// `std::fs::write` creates the file under the process umask and only narrows it
/// afterwards, which leaves a window where any local user can read a private
/// key. Creating the file with `create_new` and an explicit mode closes that
/// window and also refuses to clobber an existing key.
fn write_secret_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(SECRET_FILE_MODE);
    }
    let mut file = options
        .open(path)
        .map_err(|e| anyhow::anyhow!("create key file {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| anyhow::anyhow!("write key file {}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| anyhow::anyhow!("sync key file {}: {e}", path.display()))?;
    Ok(())
}

/// Read a persisted key file, asserting it is a regular file of the expected
/// length and, for secrets, that its mode was not widened after creation.
fn read_key_file(path: &Path, expected_len: usize, secret: bool) -> anyhow::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| anyhow::anyhow!("inspect key file {}: {e}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "key file {} is not a regular file",
        path.display()
    );
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode == SECRET_FILE_MODE,
            "key file {} has mode {:o}, expected {:o}",
            path.display(),
            mode,
            SECRET_FILE_MODE
        );
    }
    #[cfg(not(unix))]
    let _ = secret;

    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read key file {}: {e}", path.display()))?;
    anyhow::ensure!(
        bytes.len() == expected_len,
        "key file {} has length {}, expected {}",
        path.display(),
        bytes.len(),
        expected_len
    );
    Ok(bytes)
}

/// Serializes keypair generation across threads in one process.
///
/// Without this lock, concurrent callers that all observe the key files as
/// missing would each generate a *different* keypair and clobber the files on
/// disk (a TOCTOU race). A later caller could then read a keypair that no
/// longer matches data already encrypted to a previously generated public key.
/// Cross-process safety comes from `write_secret_file` using `create_new`.
static KEYGEN_LOCK: Mutex<()> = Mutex::new(());

/// The default key directory for user-facing tools: `~/.podmesh`.
pub fn default_key_dir() -> anyhow::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(KEY_DIR))
}

/// Load, or create on first use, the Ed25519 signing keypair stored in `dir`.
///
/// The directory is an explicit parameter rather than process-wide state so
/// that several components in one process — an agent and a proxy in an
/// integration test, for instance — each keep a distinct identity.
pub fn load_or_create_signing_keypair(dir: &Path) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    ensure_key_dir(dir)?;
    let pub_path = dir.join(PUBKEY_FILE);
    let priv_path = dir.join(PRIVKEY_FILE);
    let _guard = KEYGEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if pub_path.exists() && priv_path.exists() {
        let pubb = read_key_file(&pub_path, ED25519_PUBLIC_KEY_SIZE, false)?;
        let privb = read_key_file(&priv_path, ED25519_PRIVATE_KEY_SIZE, true)?;
        // A public key that does not belong to the private key beside it
        // produces signatures that verify under nobody; catch the mismatch here
        // rather than at the first failed handshake.
        let derived = signing_key_from(&privb)?.verifying_key().to_bytes();
        anyhow::ensure!(
            derived.as_slice() == pubb.as_slice(),
            "signing key files in {} do not form a keypair",
            dir.display()
        );
        return Ok((pubb, privb));
    }

    let (pubb, privb) = generate_signing_keypair();
    write_secret_file(&priv_path, &privb)?;
    write_secret_file(&pub_path, &pubb)?;
    Ok((pubb, privb))
}

/// Load, or create on first use, the X25519 KEM keypair stored in `dir`.
pub fn load_or_create_kem_keypair(dir: &Path) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    ensure_key_dir(dir)?;
    let pub_path = dir.join(KEM_PUBFILE);
    let priv_path = dir.join(KEM_PRIVFILE);
    let _guard = KEYGEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if pub_path.exists() && priv_path.exists() {
        let pubb = read_key_file(&pub_path, X25519_PUBLIC_KEY_SIZE, false)?;
        let privb = read_key_file(&priv_path, X25519_PRIVATE_KEY_SIZE, true)?;
        let derived =
            X25519PublicKey::from(&StaticSecret::from(kem_key_bytes(&privb, "private key")?));
        anyhow::ensure!(
            derived.as_bytes().as_slice() == pubb.as_slice(),
            "KEM key files in {} do not form a keypair",
            dir.display()
        );
        return Ok((pubb, privb));
    }

    let (pubb, privb) = generate_kem_keypair();
    write_secret_file(&priv_path, &privb)?;
    write_secret_file(&pub_path, &pubb)?;
    Ok((pubb, privb))
}

/// Generate a fresh, unpersisted Ed25519 keypair as `(public, private)`.
pub fn generate_signing_keypair() -> (Vec<u8>, Vec<u8>) {
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    (
        signing_key.verifying_key().to_bytes().to_vec(),
        signing_key.to_bytes().to_vec(),
    )
}

/// Generate a fresh, unpersisted X25519 keypair as `(public, private)`.
pub fn generate_kem_keypair() -> (Vec<u8>, Vec<u8>) {
    let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    (
        X25519PublicKey::from(&secret).as_bytes().to_vec(),
        secret.as_bytes().to_vec(),
    )
}

/// Validate an X25519 public key by checking its size.
pub fn validate_kem_pubkey(pub_bytes: &[u8]) -> anyhow::Result<()> {
    if pub_bytes.len() != X25519_PUBLIC_KEY_SIZE {
        anyhow::bail!(
            "Invalid X25519 public key: expected {} bytes, got {}",
            X25519_PUBLIC_KEY_SIZE,
            pub_bytes.len()
        );
    }
    Ok(())
}

fn kem_key_bytes(bytes: &[u8], what: &str) -> anyhow::Result<[u8; X25519_PUBLIC_KEY_SIZE]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid {what}: expected {X25519_PUBLIC_KEY_SIZE} bytes"))
}

/// Derive the AEAD key for a sealed blob.
///
/// The raw X25519 output is a curve coordinate, not a uniformly distributed key,
/// and on its own it binds nothing about who the parties are. The derivation
/// therefore hashes the shared secret together with both public keys under a
/// fixed context string, so a key is usable only for the exact
/// (ephemeral, recipient) pair it was produced for.
fn derive_sealed_key(
    shared_secret: &[u8],
    ephemeral_pub: &[u8],
    recipient_pub: &[u8],
) -> Zeroizing<[u8; 32]> {
    let mut transcript = Zeroizing::new(Vec::with_capacity(
        shared_secret.len() + ephemeral_pub.len() + recipient_pub.len(),
    ));
    transcript.extend_from_slice(shared_secret);
    transcript.extend_from_slice(ephemeral_pub);
    transcript.extend_from_slice(recipient_pub);
    Zeroizing::new(blake3::derive_key(KEM_KDF_CONTEXT, &transcript))
}

/// Perform X25519 key agreement using an ephemeral secret key.
/// Returns (ephemeral_public_key_bytes, shared_secret_bytes).
///
/// The shared secret is rejected when the recipient key drives it to the
/// all-zero point, which is what a small-order or low-order public key does. A
/// caller that skipped this check would encrypt under a key the recipient's peer
/// can predict, so the check is enforced here rather than left to callers.
pub fn encapsulate_to_pubkey(pub_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, Zeroizing<Vec<u8>>)> {
    let recipient_pub = kem_key_bytes(pub_bytes, "recipient public key")?;
    let recipient_public = X25519PublicKey::from(recipient_pub);

    // Generate ephemeral keypair for this encryption
    let ephemeral_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
    let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);

    // Perform X25519 key agreement
    let shared_secret = ephemeral_secret.diffie_hellman(&recipient_public);
    anyhow::ensure!(
        shared_secret.was_contributory(),
        "recipient X25519 public key is low order and yields a predictable shared secret"
    );

    Ok((
        ephemeral_public.as_bytes().to_vec(),
        Zeroizing::new(shared_secret.as_bytes().to_vec()),
    ))
}

/// Decapsulate: perform X25519 key agreement using our static private key and the sender's ephemeral public key.
/// Returns the shared secret.
pub fn decapsulate_share(
    priv_bytes: &[u8],
    ephemeral_pub_bytes: &[u8],
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let priv_arr = kem_key_bytes(priv_bytes, "private key")?;
    let ephemeral_pub_arr = kem_key_bytes(ephemeral_pub_bytes, "ephemeral public key")?;

    let our_secret = StaticSecret::from(priv_arr);
    let their_public = X25519PublicKey::from(ephemeral_pub_arr);

    let shared_secret = our_secret.diffie_hellman(&their_public);
    anyhow::ensure!(
        shared_secret.was_contributory(),
        "sender X25519 ephemeral key is low order and yields a predictable shared secret"
    );

    Ok(Zeroizing::new(shared_secret.as_bytes().to_vec()))
}

pub type EncryptedManifest = (Vec<u8>, Vec<u8>, [u8; 32], [u8; 24]);

pub fn encrypt_manifest(manifest_json: &serde_json::Value) -> anyhow::Result<EncryptedManifest> {
    let mut sym = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut sym);
    let plaintext = serde_json::to_vec(manifest_json)?;
    let (ciphertext, nonce_bytes) = encrypt_payload_with_key(&sym, &plaintext)?;
    Ok((ciphertext, nonce_bytes.to_vec(), sym, nonce_bytes))
}

/// Encrypt arbitrary bytes with a caller-owned 256-bit DEK and a fresh nonce.
pub fn encrypt_payload_with_key(
    key: &[u8; 32],
    payload: &[u8],
) -> anyhow::Result<(Vec<u8>, [u8; 24])> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("invalid key length for XChaCha20-Poly1305: {}", e))?;
    let mut nonce_bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(&nonce, payload)
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt error: {}", e))?;
    Ok((ciphertext, nonce_bytes))
}

/// Decrypt bytes produced by [`encrypt_payload_with_key`].
pub fn decrypt_payload_with_key(
    key: &[u8; 32],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("invalid key length for XChaCha20-Poly1305: {}", e))?;
    let nonce_array: [u8; 24] = nonce_bytes.try_into().map_err(|_| {
        anyhow::anyhow!("invalid nonce length: {} (expected 24)", nonce_bytes.len())
    })?;
    cipher
        .decrypt(&XNonce::from(nonce_array), ciphertext)
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt error: {}", e))
}

/// Encrypt an arbitrary payload to a recipient's X25519 public key.
///
/// Blob format (version 0x04):
/// `[version 1][ephemeral_pubkey 32][nonce 24][ctlen u32 BE][ciphertext ctlen]`
///
/// The whole header is passed as AEAD associated data, so the version byte, the
/// ephemeral public key, the nonce and the declared length are all covered by
/// the Poly1305 tag and cannot be rewritten by a relay.
pub fn encrypt_payload_for_recipient(
    recipient_pub: &[u8],
    payload: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let (ephemeral_pub, shared_secret) = encapsulate_to_pubkey(recipient_pub)?;
    let key = derive_sealed_key(&shared_secret, &ephemeral_pub, recipient_pub);
    let cipher = XChaCha20Poly1305::new_from_slice(&key[..])
        .map_err(|e| anyhow::anyhow!("invalid key length for XChaCha20-Poly1305: {}", e))?;

    let mut nonce_bytes = [0u8; XCHACHA20_NONCE_SIZE];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);

    let ciphertext_len = u32::try_from(
        payload
            .len()
            .checked_add(POLY1305_TAG_SIZE)
            .ok_or_else(|| anyhow::anyhow!("payload too large to seal"))?,
    )
    .map_err(|_| anyhow::anyhow!("payload too large to seal"))?;

    let mut header = Vec::with_capacity(SEALED_HEADER_LEN);
    header.push(SEALED_BLOB_VERSION);
    header.extend_from_slice(&ephemeral_pub);
    header.extend_from_slice(&nonce_bytes);
    header.extend_from_slice(&ciphertext_len.to_be_bytes());

    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: payload,
                aad: &header,
            },
        )
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt error: {}", e))?;
    anyhow::ensure!(
        ciphertext.len() == ciphertext_len as usize,
        "sealed ciphertext length disagrees with the authenticated header"
    );

    let mut blob = Vec::with_capacity(SEALED_HEADER_LEN + ciphertext.len());
    blob.extend_from_slice(&header);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Reverse of [`encrypt_payload_for_recipient`].
///
/// The blob length must match the declared ciphertext length exactly; trailing
/// bytes are refused rather than ignored, so a sealed blob has one and only one
/// valid encoding.
pub fn decrypt_payload_from_recipient_blob(
    blob: &[u8],
    priv_kem_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(blob.len() >= SEALED_HEADER_LEN, "sealed blob too short");
    anyhow::ensure!(
        blob[0] == SEALED_BLOB_VERSION,
        "unsupported sealed-blob version: {}",
        blob[0]
    );

    let ephemeral_pub = &blob[1..33];
    let nonce_bytes: [u8; XCHACHA20_NONCE_SIZE] = blob[33..57]
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid sealed-blob nonce"))?;
    let declared_len = u32::from_be_bytes([blob[57], blob[58], blob[59], blob[60]]) as usize;
    let ciphertext = &blob[SEALED_HEADER_LEN..];
    anyhow::ensure!(
        ciphertext.len() == declared_len,
        "sealed blob length {} disagrees with declared ciphertext length {}",
        ciphertext.len(),
        declared_len
    );

    let shared = decapsulate_share(priv_kem_bytes, ephemeral_pub)?;
    let recipient_secret = StaticSecret::from(kem_key_bytes(priv_kem_bytes, "private key")?);
    let recipient_public = X25519PublicKey::from(&recipient_secret);
    let key = derive_sealed_key(&shared, ephemeral_pub, recipient_public.as_bytes());
    let cipher = XChaCha20Poly1305::new_from_slice(&key[..])
        .map_err(|e| anyhow::anyhow!("key error: {}", e))?;

    cipher
        .decrypt(
            &XNonce::from(nonce_bytes),
            chacha20poly1305::aead::Payload {
                msg: ciphertext,
                aad: &blob[..SEALED_HEADER_LEN],
            },
        )
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt error: {}", e))
}

/// Decrypt a manifest ciphertext produced by `encrypt_manifest` using the symmetric key and nonce.
pub fn decrypt_manifest(
    sym: &[u8; 32],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
) -> anyhow::Result<Vec<u8>> {
    decrypt_payload_with_key(sym, nonce_bytes, ciphertext)
}

fn signing_key_from(sk_bytes: &[u8]) -> anyhow::Result<SigningKey> {
    let sk_arr: [u8; ED25519_PRIVATE_KEY_SIZE] = sk_bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid private key size: expected {}, got {}",
            ED25519_PRIVATE_KEY_SIZE,
            sk_bytes.len()
        )
    })?;
    Ok(SigningKey::from_bytes(&sk_arr))
}

/// Load an Ed25519 verifying key, refusing keys of small order.
///
/// A low-order verifying key admits signatures that verify under more than one
/// identity. Because a public key *is* the identity everywhere in Podmesh, such
/// a key would let one party repudiate a message or claim another's, so it is
/// rejected before any verification is attempted.
fn verifying_key_from(pub_bytes: &[u8]) -> anyhow::Result<VerifyingKey> {
    let pub_arr: [u8; ED25519_PUBLIC_KEY_SIZE] = pub_bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid public key size: expected {}, got {}",
            ED25519_PUBLIC_KEY_SIZE,
            pub_bytes.len()
        )
    })?;
    let verifying_key = VerifyingKey::from_bytes(&pub_arr)
        .map_err(|e| anyhow::anyhow!("Invalid public key: {}", e))?;
    anyhow::ensure!(
        !verifying_key.is_weak(),
        "Ed25519 public key is of small order"
    );
    Ok(verifying_key)
}

/// Sign `message` under `domain`. The domain label is bound into the signed
/// bytes, so the result is not a valid signature for any other message type.
pub fn sign_domain(
    sk_bytes: &[u8],
    domain: SignatureDomain,
    message: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let signing_key = signing_key_from(sk_bytes)?;
    Ok(signing_key.sign(&domain.bind(message)).to_bytes().to_vec())
}

/// Verify a signature produced by [`sign_domain`].
///
/// Uses `verify_strict`, which rejects torsioned `R` components and therefore
/// admits exactly one signature per (key, message) pair.
pub fn verify_domain(
    pub_bytes: &[u8],
    domain: SignatureDomain,
    message: &[u8],
    sig_bytes: &[u8],
) -> anyhow::Result<()> {
    let sig_arr: [u8; ED25519_SIGNATURE_SIZE] = sig_bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid signature size: expected {}, got {}",
            ED25519_SIGNATURE_SIZE,
            sig_bytes.len()
        )
    })?;
    verifying_key_from(pub_bytes)?
        .verify_strict(&domain.bind(message), &Signature::from_bytes(&sig_arr))
        .map_err(|e| anyhow::anyhow!("signature verification failed: {}", e))
}

/// Sign a domain-bound message and return `(signature_b64, public_key_b64)`.
pub fn sign_domain_b64(
    sk_bytes: &[u8],
    pk_bytes: &[u8],
    domain: SignatureDomain,
    message: &[u8],
) -> anyhow::Result<(String, String)> {
    anyhow::ensure!(
        pk_bytes.len() == ED25519_PUBLIC_KEY_SIZE,
        "Invalid public key size: expected {}, got {}",
        ED25519_PUBLIC_KEY_SIZE,
        pk_bytes.len()
    );
    let signature = sign_domain(sk_bytes, domain, message)?;
    Ok((b64_encode(&signature), b64_encode(pk_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_key_roundtrip_and_tamper_rejection() {
        let key = [42u8; 32];
        let (mut ciphertext, nonce) =
            encrypt_payload_with_key(&key, b"encrypted workload").unwrap();
        assert_eq!(
            decrypt_payload_with_key(&key, &nonce, &ciphertext).unwrap(),
            b"encrypted workload"
        );
        ciphertext[0] ^= 1;
        assert!(decrypt_payload_with_key(&key, &nonce, &ciphertext).is_err());
    }

    #[test]
    fn test_sign_and_verify_envelope() {
        let (pubb, skb) = generate_signing_keypair();
        let payload = b"this is a test envelope payload";
        let (sig_b64, pub_b64) =
            sign_domain_b64(&skb, &pubb, SignatureDomain::CapacityOffer, payload).expect("sign");
        let sig_bytes = b64_decode(&sig_b64).expect("b64 decode sig");
        let pub_bytes = b64_decode(&pub_b64).expect("b64 decode pub");
        verify_domain(
            &pub_bytes,
            SignatureDomain::CapacityOffer,
            payload,
            &sig_bytes,
        )
        .expect("verify");

        // A mutated signature must not verify.
        let mut mutated = sig_bytes.clone();
        mutated[0] ^= 0xff;
        assert!(
            verify_domain(
                &pub_bytes,
                SignatureDomain::CapacityOffer,
                payload,
                &mutated
            )
            .is_err()
        );
    }

    #[test]
    fn signature_does_not_transfer_across_domains() {
        let (pubb, skb) = generate_signing_keypair();
        let message = b"identical canonical bytes";
        let signature =
            sign_domain(&skb, SignatureDomain::CapacityQuery, message).expect("sign query");
        assert!(verify_domain(&pubb, SignatureDomain::CapacityQuery, message, &signature).is_ok());
        assert!(
            verify_domain(&pubb, SignatureDomain::CapacityOffer, message, &signature).is_err(),
            "a signature must not verify under a different message domain"
        );
    }

    #[test]
    fn low_order_recipient_key_is_refused() {
        // The canonical order-8 point: X25519 against it always yields zero, so
        // the derived key would be identical for every sender.
        let low_order = [0u8; X25519_PUBLIC_KEY_SIZE];
        let error = encrypt_payload_for_recipient(&low_order, b"secret").unwrap_err();
        assert!(
            error.to_string().contains("low order"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn sealed_blob_rejects_trailing_bytes_and_header_tampering() {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = X25519PublicKey::from(&secret);
        let privb = secret.as_bytes().to_vec();

        let blob =
            encrypt_payload_for_recipient(public.as_bytes(), b"sealed payload").expect("encrypt");
        assert_eq!(
            decrypt_payload_from_recipient_blob(&blob, &privb).expect("decrypt"),
            b"sealed payload"
        );

        let mut padded = blob.clone();
        padded.push(0);
        assert!(decrypt_payload_from_recipient_blob(&padded, &privb).is_err());

        // The nonce lives in the header and is covered by the AEAD tag.
        let mut tampered = blob.clone();
        tampered[40] ^= 1;
        assert!(decrypt_payload_from_recipient_blob(&tampered, &privb).is_err());
    }

    #[test]
    fn test_kem_encapsulate_decapsulate_roundtrip() {
        // Generate a static X25519 keypair
        let rng = rand::rngs::OsRng;
        let secret = StaticSecret::random_from_rng(rng);
        let public = X25519PublicKey::from(&secret);
        let pubb = public.as_bytes().to_vec();
        let privb = secret.as_bytes().to_vec();

        // Encapsulate
        let (ephemeral_pub, shared_enc) = encapsulate_to_pubkey(&pubb).expect("encapsulate");
        // Decapsulate and ensure secrets match
        let shared_dec = decapsulate_share(&privb, &ephemeral_pub).expect("decapsulate");
        assert_eq!(&shared_enc[..], &shared_dec[..]);
    }

    #[test]
    fn test_recipient_blob_roundtrip() {
        let rng = rand::rngs::OsRng;
        let secret = StaticSecret::random_from_rng(rng);
        let public = X25519PublicKey::from(&secret);
        let pubb = public.as_bytes().to_vec();
        let privb = secret.as_bytes().to_vec();

        let payload = b"hello recipient payload";
        let blob = encrypt_payload_for_recipient(&pubb, payload).expect("encrypt recipient blob");
        let recovered =
            decrypt_payload_from_recipient_blob(&blob, &privb).expect("decrypt recipient blob");
        assert_eq!(recovered, payload);
    }

    #[test]
    fn test_encrypt_manifest_roundtrip() {
        let manifest = serde_json::json!({"name": "test", "version": "1.0"});
        let (ciphertext, nonce_vec, sym, _nonce_arr) =
            encrypt_manifest(&manifest).expect("encrypt");
        let decrypted = decrypt_manifest(&sym, &nonce_vec, &ciphertext).expect("decrypt");
        let recovered: serde_json::Value = serde_json::from_slice(&decrypted).expect("parse json");
        assert_eq!(recovered, manifest);
    }

    #[test]
    fn test_key_sizes() {
        let (pub_bytes, priv_bytes) = generate_signing_keypair();
        assert_eq!(pub_bytes.len(), ED25519_PUBLIC_KEY_SIZE);
        assert_eq!(priv_bytes.len(), ED25519_PRIVATE_KEY_SIZE);
    }
}
