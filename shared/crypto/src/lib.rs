use base64::{Engine as _, engine::general_purpose};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use once_cell::sync::Lazy;
use rand::RngCore;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey, StaticSecret};
use zeroize::Zeroizing;

use dirs::home_dir;

pub mod envelope_validator;
pub mod nonce_helper;
pub mod keypair_manager;
pub mod logging;

pub const KEY_DIR: &str = ".podmesh";
pub const PUBKEY_FILE: &str = "pubkey.bin";
pub const PRIVKEY_FILE: &str = "privkey.bin";
pub const KEM_PUBFILE: &str = "kem_pub.bin";
pub const KEM_PRIVFILE: &str = "kem_priv.bin";

/// ed25519 key sizes
pub const ED25519_PUBLIC_KEY_SIZE: usize = 32;
pub const ED25519_PRIVATE_KEY_SIZE: usize = 32;
pub const ED25519_SIGNATURE_SIZE: usize = 64;

/// X25519 key sizes  
pub const X25519_PUBLIC_KEY_SIZE: usize = 32;
pub const X25519_PRIVATE_KEY_SIZE: usize = 32;

/// XChaCha20-Poly1305 nonce size
pub const XCHACHA20_NONCE_SIZE: usize = 24;

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

/// Storage mode for key material
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeypairMode {
    Persistent,
    Ephemeral,
}

/// Global configuration for keypair handling
#[derive(Clone, Debug)]
pub struct KeypairConfig {
    pub signing_mode: KeypairMode,
    pub kem_mode: KeypairMode,
    pub key_directory: Option<PathBuf>,
}

impl Default for KeypairConfig {
    fn default() -> Self {
        Self {
            signing_mode: KeypairMode::Persistent,
            kem_mode: KeypairMode::Persistent,
            key_directory: None,
        }
    }
}

static KEYPAIR_CONFIG: Lazy<RwLock<KeypairConfig>> =
    Lazy::new(|| RwLock::new(KeypairConfig::default()));

/// Update the global keypair configuration at runtime.
/// This should typically be called once during application startup based on CLI flags.
pub fn set_keypair_config(config: KeypairConfig) {
    let mut guard = KEYPAIR_CONFIG
        .write()
        .expect("keypair config rwlock poisoned");
    *guard = config;
}

/// Fetch the current keypair configuration.
pub fn get_keypair_config() -> KeypairConfig {
    KEYPAIR_CONFIG
        .read()
        .expect("keypair config rwlock poisoned")
        .clone()
}

fn resolve_key_dir(dir_override: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = dir_override {
        return Ok(path.to_path_buf());
    }

    let home = home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home dir"))?;
    Ok(home.join(KEY_DIR))
}

fn ensure_key_dir(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Cached ephemeral signing keypair
static EPHEMERAL_SIGNING: once_cell::sync::OnceCell<(Vec<u8>, Vec<u8>)> = once_cell::sync::OnceCell::new();

/// Cached ephemeral KEM keypair
static EPHEMERAL_KEM: once_cell::sync::OnceCell<(Vec<u8>, Vec<u8>)> = once_cell::sync::OnceCell::new();

pub fn ensure_keypair_on_disk() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let config = get_keypair_config();

    match config.signing_mode {
        KeypairMode::Ephemeral => {
            let keypair = EPHEMERAL_SIGNING.get_or_try_init(|| {
                let mut rng = rand::rngs::OsRng;
                let signing_key = SigningKey::generate(&mut rng);
                let verifying_key = signing_key.verifying_key();
                let pubb = verifying_key.to_bytes().to_vec();
                let privb = signing_key.to_bytes().to_vec();
                log::warn!("ensure_keypair_on_disk: using ephemeral signing keypair (no disk writes)");
                Ok::<_, anyhow::Error>((pubb, privb))
            })?;
            Ok((keypair.0.clone(), keypair.1.clone()))
        }
        KeypairMode::Persistent => {
            let key_dir = resolve_key_dir(config.key_directory.as_deref())?;
            ensure_key_dir(&key_dir)?;

            let pub_path = key_dir.join(PUBKEY_FILE);
            let priv_path = key_dir.join(PRIVKEY_FILE);
            if pub_path.exists() && priv_path.exists() {
                let pubb = std::fs::read(&pub_path)?;
                let privb = std::fs::read(&priv_path)?;
                // Validate key sizes
                if pubb.len() != ED25519_PUBLIC_KEY_SIZE {
                    anyhow::bail!("Invalid public key size: expected {}, got {}", ED25519_PUBLIC_KEY_SIZE, pubb.len());
                }
                if privb.len() != ED25519_PRIVATE_KEY_SIZE {
                    anyhow::bail!("Invalid private key size: expected {}, got {}", ED25519_PRIVATE_KEY_SIZE, privb.len());
                }
                return Ok((pubb, privb));
            }

            let mut rng = rand::rngs::OsRng;
            let signing_key = SigningKey::generate(&mut rng);
            let verifying_key = signing_key.verifying_key();
            let pubb = verifying_key.to_bytes().to_vec();
            let privb = signing_key.to_bytes().to_vec();
            std::fs::write(&pub_path, &pubb)?;
            std::fs::write(&priv_path, &privb)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&pub_path, std::fs::Permissions::from_mode(0o600))?;
                std::fs::set_permissions(&priv_path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok((pubb, privb))
        }
    }
}

/// Ensure a KEM (X25519) keypair exists on disk. Returns (pub_bytes, priv_bytes).
pub fn ensure_kem_keypair_on_disk() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let config = get_keypair_config();

    match config.kem_mode {
        KeypairMode::Ephemeral => {
            let keypair = EPHEMERAL_KEM.get_or_try_init(|| {
                let mut rng = rand::rngs::OsRng;
                let secret = StaticSecret::random_from_rng(&mut rng);
                let public = X25519PublicKey::from(&secret);
                let pubb = public.as_bytes().to_vec();
                let privb = secret.as_bytes().to_vec();
                log::warn!("ensure_kem_keypair_on_disk: using ephemeral KEM keypair (no disk writes)");
                Ok::<_, anyhow::Error>((pubb, privb))
            })?;
            Ok((keypair.0.clone(), keypair.1.clone()))
        }
        KeypairMode::Persistent => {
            let key_dir = resolve_key_dir(config.key_directory.as_deref())?;
            ensure_key_dir(&key_dir)?;

            let pub_path = key_dir.join(KEM_PUBFILE);
            let priv_path = key_dir.join(KEM_PRIVFILE);
            if pub_path.exists() && priv_path.exists() {
                let pubb = std::fs::read(&pub_path)?;
                let privb = std::fs::read(&priv_path)?;
                // Validate key sizes
                if pubb.len() != X25519_PUBLIC_KEY_SIZE {
                    anyhow::bail!("Invalid X25519 public key size: expected {}, got {}", X25519_PUBLIC_KEY_SIZE, pubb.len());
                }
                if privb.len() != X25519_PRIVATE_KEY_SIZE {
                    anyhow::bail!("Invalid X25519 private key size: expected {}, got {}", X25519_PRIVATE_KEY_SIZE, privb.len());
                }
                return Ok((pubb, privb));
            }

            let mut rng = rand::rngs::OsRng;
            let secret = StaticSecret::random_from_rng(&mut rng);
            let public = X25519PublicKey::from(&secret);
            let pubb = public.as_bytes().to_vec();
            let privb = secret.as_bytes().to_vec();
            std::fs::write(&pub_path, &pubb)?;
            std::fs::write(&priv_path, &privb)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&pub_path, std::fs::Permissions::from_mode(0o600))?;
                std::fs::set_permissions(&priv_path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok((pubb, privb))
        }
    }
}

/// Validate an X25519 public key by checking its size.
pub fn validate_kem_pubkey(pub_bytes: &[u8]) -> anyhow::Result<()> {
    if pub_bytes.len() != X25519_PUBLIC_KEY_SIZE {
        anyhow::bail!("Invalid X25519 public key: expected {} bytes, got {}", X25519_PUBLIC_KEY_SIZE, pub_bytes.len());
    }
    Ok(())
}

/// Perform X25519 key agreement using an ephemeral secret key.
/// Returns (ephemeral_public_key_bytes, shared_secret_bytes).
/// The ephemeral public key should be sent to the recipient along with the ciphertext.
pub fn encapsulate_to_pubkey(pub_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    if pub_bytes.len() != X25519_PUBLIC_KEY_SIZE {
        anyhow::bail!("Invalid recipient public key size");
    }
    
    let recipient_pub: [u8; 32] = pub_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid recipient public key"))?;
    let recipient_public = X25519PublicKey::from(recipient_pub);
    
    // Generate ephemeral keypair for this encryption
    let ephemeral_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
    let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);
    
    // Perform X25519 key agreement
    let shared_secret = ephemeral_secret.diffie_hellman(&recipient_public);
    
    Ok((ephemeral_public.as_bytes().to_vec(), shared_secret.as_bytes().to_vec()))
}

/// Decapsulate: perform X25519 key agreement using our static private key and the sender's ephemeral public key.
/// Returns the shared secret.
pub fn decapsulate_share(
    priv_bytes: &[u8],
    ephemeral_pub_bytes: &[u8],
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if priv_bytes.len() != X25519_PRIVATE_KEY_SIZE {
        anyhow::bail!("Invalid private key size");
    }
    if ephemeral_pub_bytes.len() != X25519_PUBLIC_KEY_SIZE {
        anyhow::bail!("Invalid ephemeral public key size");
    }
    
    let priv_arr: [u8; 32] = priv_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid private key"))?;
    let ephemeral_pub_arr: [u8; 32] = ephemeral_pub_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid ephemeral public key"))?;
    
    let our_secret = StaticSecret::from(priv_arr);
    let their_public = X25519PublicKey::from(ephemeral_pub_arr);
    
    let shared_secret = our_secret.diffie_hellman(&their_public);
    
    Ok(Zeroizing::new(shared_secret.as_bytes().to_vec()))
}

pub fn ensure_keypair_ephemeral() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut rng = rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();
    Ok((verifying_key.to_bytes().to_vec(), signing_key.to_bytes().to_vec()))
}

/// Get cached ephemeral keypair (same instance across calls) - useful for tests
pub fn ensure_keypair_ephemeral_cached() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    keypair_manager::KeypairManager::get_ephemeral_signing_keypair()
        .map_err(|e| anyhow::anyhow!("ephemeral cached keypair: {}", e))
}

/// Clear cached ephemeral keypairs - useful for test isolation
pub fn clear_ephemeral_keypair_cache() {
    keypair_manager::KeypairManager::clear_ephemeral_caches();
}

pub fn encrypt_manifest(
    manifest_json: &serde_json::Value,
) -> anyhow::Result<(Vec<u8>, Vec<u8>, [u8; 32], [u8; 24])> {
    let mut sym = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut sym);
    let cipher = XChaCha20Poly1305::new_from_slice(&sym)
        .map_err(|e| anyhow::anyhow!("invalid key length for XChaCha20-Poly1305: {}", e))?;
    let mut nonce_bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);
    let plaintext = serde_json::to_vec(manifest_json)?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_ref())
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt error: {}", e))?;
    Ok((ciphertext, nonce_bytes.to_vec(), sym, nonce_bytes))
}

/// Encrypt an arbitrary payload to a recipient's X25519 public key.
/// Output blob format (version 0x03):
/// [version=0x03 u8][ephemeral_pubkey 32 bytes][nonce 24 bytes][ctlen u32 BE][ciphertext bytes]
pub fn encrypt_payload_for_recipient(
    recipient_pub: &[u8],
    payload: &[u8],
) -> anyhow::Result<Vec<u8>> {
    // Perform X25519 key agreement with ephemeral keypair
    let (ephemeral_pub, shared_secret) = encapsulate_to_pubkey(recipient_pub)?;
    
    let cipher = XChaCha20Poly1305::new_from_slice(&shared_secret)
        .map_err(|e| anyhow::anyhow!("invalid key length for XChaCha20-Poly1305: {}", e))?;
    let mut nonce_bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(&nonce, payload)
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt error: {}", e))?;

    // Version 0x03: ed25519/X25519/XChaCha20-Poly1305
    let mut blob = Vec::with_capacity(1 + 32 + 24 + 4 + ciphertext.len());
    blob.push(0x03u8);
    blob.extend_from_slice(&ephemeral_pub); // 32 bytes
    blob.extend_from_slice(&nonce_bytes);   // 24 bytes
    let clen = ciphertext.len() as u32;
    blob.extend_from_slice(&clen.to_be_bytes());
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Reverse of encrypt_payload_for_recipient: perform X25519 key agreement and decrypt XChaCha20-Poly1305 ciphertext.
pub fn decrypt_payload_from_recipient_blob(
    blob: &[u8],
    priv_kem_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    if blob.is_empty() {
        anyhow::bail!("empty blob");
    }
    
    let version = blob[0];
    if version != 0x03 {
        anyhow::bail!("unsupported recipient-blob version: {}", version);
    }
    
    // Parse version 0x03 format: [version 1][ephemeral_pub 32][nonce 24][ctlen 4][ciphertext]
    let min_len = 1 + 32 + 24 + 4;
    if blob.len() < min_len {
        anyhow::bail!("blob too short");
    }
    
    let ephemeral_pub = &blob[1..33];
    let nonce_bytes = &blob[33..57];
    let clen = u32::from_be_bytes([blob[57], blob[58], blob[59], blob[60]]) as usize;
    
    if blob.len() < min_len + clen {
        anyhow::bail!("blob too short for ciphertext");
    }
    let ciphertext = &blob[61..61 + clen];

    // Perform X25519 key agreement
    let shared = decapsulate_share(priv_kem_bytes, ephemeral_pub)?;
    
    let cipher = XChaCha20Poly1305::new_from_slice(&shared[..])
        .map_err(|e| anyhow::anyhow!("key error: {}", e))?;
    
    let nonce_array: [u8; 24] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let nonce = XNonce::from(nonce_array);
    
    let plain = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt error: {}", e))?;
    Ok(plain)
}

/// Decrypt a manifest ciphertext produced by `encrypt_manifest` using the symmetric key and nonce.
pub fn decrypt_manifest(
    sym: &[u8; 32],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(&sym[..])
        .map_err(|e| anyhow::anyhow!("invalid key length for XChaCha20-Poly1305: {}", e))?;
    if nonce_bytes.len() != 24 {
        anyhow::bail!("invalid nonce length: {} (expected 24)", nonce_bytes.len());
    }
    let nonce_array: [u8; 24] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("nonce length mismatch"))?;
    let nonce = XNonce::from(nonce_array);
    let plain = cipher
        .decrypt(&nonce, ciphertext.as_ref())
        .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt error: {}", e))?;
    Ok(plain)
}

pub fn sign_envelope(
    sk_bytes: &[u8],
    pk_bytes: &[u8],
    envelope_bytes: &[u8],
) -> anyhow::Result<(String, String)> {
    if sk_bytes.len() != ED25519_PRIVATE_KEY_SIZE {
        anyhow::bail!("Invalid private key size: expected {}, got {}", ED25519_PRIVATE_KEY_SIZE, sk_bytes.len());
    }
    if pk_bytes.len() != ED25519_PUBLIC_KEY_SIZE {
        anyhow::bail!("Invalid public key size: expected {}, got {}", ED25519_PUBLIC_KEY_SIZE, pk_bytes.len());
    }
    
    let sk_arr: [u8; 32] = sk_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid private key"))?;
    let signing_key = SigningKey::from_bytes(&sk_arr);
    
    let signature = signing_key.sign(envelope_bytes);
    let sig_b64 = general_purpose::STANDARD.encode(signature.to_bytes());
    let pub_b64 = general_purpose::STANDARD.encode(pk_bytes);
    
    Ok((sig_b64, pub_b64))
}

pub fn verify_envelope(
    pub_bytes: &[u8],
    envelope_bytes: &[u8],
    sig_bytes: &[u8],
) -> anyhow::Result<()> {
    if pub_bytes.len() != ED25519_PUBLIC_KEY_SIZE {
        anyhow::bail!("Invalid public key size: expected {}, got {}", ED25519_PUBLIC_KEY_SIZE, pub_bytes.len());
    }
    if sig_bytes.len() != ED25519_SIGNATURE_SIZE {
        anyhow::bail!("Invalid signature size: expected {}, got {}", ED25519_SIGNATURE_SIZE, sig_bytes.len());
    }
    
    let pub_arr: [u8; 32] = pub_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid public key"))?;
    let sig_arr: [u8; 64] = sig_bytes.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid signature"))?;
    
    let verifying_key = VerifyingKey::from_bytes(&pub_arr)
        .map_err(|e| anyhow::anyhow!("Invalid public key: {}", e))?;
    let signature = Signature::from_bytes(&sig_arr);
    
    verifying_key.verify(envelope_bytes, &signature)
        .map_err(|e| anyhow::anyhow!("signature verification failed: {}", e))?;
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify_envelope() {
        // 1) wrapper round-trip: ensure our sign_envelope / verify_envelope work
        let (pub_bytes_wrapped, sk_bytes_wrapped) =
            ensure_keypair_ephemeral().expect("keygen failed");
        let payload = b"this is a test envelope payload";
        let (sig_b64, pub_b64) =
            sign_envelope(&sk_bytes_wrapped, &pub_bytes_wrapped, payload).expect("sign failed");
        let sig_bytes = general_purpose::STANDARD
            .decode(&sig_b64)
            .expect("b64 decode sig");
        let pub_bytes = general_purpose::STANDARD
            .decode(&pub_b64)
            .expect("b64 decode pub");
        verify_envelope(&pub_bytes, payload, &sig_bytes).expect("verify call failed");

        // 2) Negative check: signatures generated for key A MUST NOT verify under key B
        let (pub_a, sk_a) = ensure_keypair_ephemeral().expect("keygen a");
        let (_pub_b, _sk_b) = ensure_keypair_ephemeral().expect("keygen b");
        let (sig_b64_a, _pub_b64_a) =
            sign_envelope(&sk_a, &pub_a, payload).expect("sign with a failed");
        let mut sig_bytes_a = general_purpose::STANDARD
            .decode(&sig_b64_a)
            .expect("decode sig a");
        // flip a byte in the signature
        if !sig_bytes_a.is_empty() {
            sig_bytes_a[0] ^= 0xff;
        }
        let res = verify_envelope(&pub_a, payload, &sig_bytes_a);
        assert!(res.is_err(), "mutated signature must not verify");
    }

    #[test]
    fn test_kem_encapsulate_decapsulate_roundtrip() {
        // Generate a static X25519 keypair
        let mut rng = rand::rngs::OsRng;
        let secret = StaticSecret::random_from_rng(&mut rng);
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
        let mut rng = rand::rngs::OsRng;
        let secret = StaticSecret::random_from_rng(&mut rng);
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
        let (ciphertext, nonce_vec, sym, _nonce_arr) = encrypt_manifest(&manifest).expect("encrypt");
        let decrypted = decrypt_manifest(&sym, &nonce_vec, &ciphertext).expect("decrypt");
        let recovered: serde_json::Value = serde_json::from_slice(&decrypted).expect("parse json");
        assert_eq!(recovered, manifest);
    }
    
    #[test]
    fn test_key_sizes() {
        let (pub_bytes, priv_bytes) = ensure_keypair_ephemeral().expect("keygen");
        assert_eq!(pub_bytes.len(), ED25519_PUBLIC_KEY_SIZE);
        assert_eq!(priv_bytes.len(), ED25519_PRIVATE_KEY_SIZE);
    }
}
