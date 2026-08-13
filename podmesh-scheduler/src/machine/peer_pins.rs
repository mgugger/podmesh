//! Pinned identities for peer schedulers.
//!
//! A peer's `EndpointRecord` is self-signed: the key that validates it is
//! carried inside it. Verifying the signature therefore proves the record is
//! internally consistent and nothing more — anyone able to answer a configured
//! peer URL can mint a record over an identity of their choosing. Since a
//! discovered peer is admitted to the gossip allowlist *and* trusted as a relay
//! grant issuer, that would be a complete takeover of the control plane from a
//! single spoofed HTTP response.
//!
//! Each peer URL is therefore bound to one identity. An operator can configure
//! the binding up front; otherwise the first successful observation is written
//! to disk and every later observation must match it. A mismatch is refused and
//! logged rather than silently replacing the pin, because a legitimate identity
//! change means an operator rebuilt that scheduler's key directory and should
//! say so explicitly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// File inside the key directory holding the pins.
pub const PEER_PINS_FILE: &str = "peer_pins.json";
/// Bound on stored pins, matched to the configured peer URL limit.
pub const MAX_PEER_PINS: usize = super::discovery::MAX_PEER_URLS;

/// The identity a peer URL is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPin {
    /// Lowercase hex Iroh endpoint id.
    pub endpoint_id: String,
    /// Base64 Ed25519 application signing key.
    pub signing_pubkey: String,
}

impl PeerPin {
    pub fn new(endpoint_id: EndpointId, signing_pubkey: &[u8]) -> Self {
        Self {
            endpoint_id: hex::encode(endpoint_id.as_bytes()),
            signing_pubkey: crypto::b64_encode(signing_pubkey),
        }
    }
}

/// Parse an operator-supplied `url=endpoint_id_hex:signing_pubkey_b64` binding.
pub fn parse_pin_argument(value: &str) -> Result<(String, PeerPin)> {
    let (url, identity) = value
        .split_once('=')
        .context("peer pin must be written as <url>=<endpoint_id_hex>:<signing_pubkey_b64>")?;
    let (endpoint_id, signing_pubkey) = identity
        .split_once(':')
        .context("peer pin identity must be written as <endpoint_id_hex>:<signing_pubkey_b64>")?;
    let decoded = hex::decode(endpoint_id).context("peer pin endpoint id must be hex")?;
    ensure!(
        decoded.len() == protocol::IROH_ENDPOINT_ID_BYTES,
        "peer pin endpoint id must be {} bytes",
        protocol::IROH_ENDPOINT_ID_BYTES
    );
    ensure!(
        crypto::b64_decode(signing_pubkey)?.len() == crypto::ED25519_PUBLIC_KEY_SIZE,
        "peer pin signing key must decode to {} bytes",
        crypto::ED25519_PUBLIC_KEY_SIZE
    );
    Ok((
        normalize_url(url),
        PeerPin {
            endpoint_id: endpoint_id.to_ascii_lowercase(),
            signing_pubkey: signing_pubkey.to_string(),
        },
    ))
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Peer URL to pinned identity, persisted beside the scheduler's own keys.
#[derive(Debug)]
pub struct PeerPins {
    path: PathBuf,
    pins: std::sync::Mutex<BTreeMap<String, PeerPin>>,
}

impl PeerPins {
    /// Load the pin file, merging in any operator-configured bindings.
    ///
    /// A configured binding always wins: it is the operator stating the
    /// identity out of band, which is strictly stronger than a remembered
    /// first observation.
    pub fn load(key_dir: &Path, configured: Vec<(String, PeerPin)>) -> Result<Self> {
        let path = key_dir.join(PEER_PINS_FILE);
        let mut pins: BTreeMap<String, PeerPin> = if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read peer pins {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("decode peer pins {}", path.display()))?
        } else {
            BTreeMap::new()
        };
        for (url, pin) in configured {
            pins.insert(url, pin);
        }
        ensure!(
            pins.len() <= MAX_PEER_PINS,
            "at most {MAX_PEER_PINS} scheduler peer pins are supported"
        );
        Ok(Self {
            path,
            pins: std::sync::Mutex::new(pins),
        })
    }

    /// Check an observed identity against the pin for `url`, pinning it on
    /// first sight. Returns an error when the observation contradicts the pin.
    pub fn accept(&self, url: &str, observed: &PeerPin) -> Result<()> {
        let url = normalize_url(url);
        let mut pins = self
            .pins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match pins.get(&url) {
            Some(pinned) if pinned == observed => Ok(()),
            Some(pinned) => bail!(
                "scheduler peer {url} now presents endpoint {} with signing key {}, but it is \
                 pinned to endpoint {} with signing key {}; delete {} only if you rebuilt that \
                 scheduler's key directory on purpose",
                observed.endpoint_id,
                observed.signing_pubkey,
                pinned.endpoint_id,
                pinned.signing_pubkey,
                self.path.display()
            ),
            None => {
                ensure!(
                    pins.len() < MAX_PEER_PINS,
                    "at most {MAX_PEER_PINS} scheduler peer pins are supported"
                );
                log::info!(
                    "pinning scheduler peer {url} to endpoint {}",
                    observed.endpoint_id
                );
                pins.insert(url, observed.clone());
                let snapshot = pins.clone();
                drop(pins);
                self.persist(&snapshot)
            }
        }
    }

    fn persist(&self, pins: &BTreeMap<String, PeerPin>) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(pins)?)
            .with_context(|| format!("write peer pins {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("commit peer pins {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(seed: u8) -> PeerPin {
        PeerPin {
            endpoint_id: hex::encode([seed; protocol::IROH_ENDPOINT_ID_BYTES]),
            signing_pubkey: crypto::b64_encode(&[seed; crypto::ED25519_PUBLIC_KEY_SIZE]),
        }
    }

    #[test]
    fn the_first_observation_is_pinned_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let pins = PeerPins::load(dir.path(), Vec::new()).unwrap();
        pins.accept("http://peer:3000", &pin(1)).unwrap();
        pins.accept("http://peer:3000/", &pin(1)).unwrap();

        let reloaded = PeerPins::load(dir.path(), Vec::new()).unwrap();
        reloaded.accept("http://peer:3000", &pin(1)).unwrap();
        let error = reloaded.accept("http://peer:3000", &pin(2)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("but it is\n                 pinned to")
                || error.to_string().contains("pinned to")
        );
    }

    #[test]
    fn a_changed_identity_is_refused_rather_than_replacing_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let pins = PeerPins::load(dir.path(), Vec::new()).unwrap();
        pins.accept("http://peer:3000", &pin(1)).unwrap();
        assert!(pins.accept("http://peer:3000", &pin(2)).is_err());
        // The original pin still stands.
        pins.accept("http://peer:3000", &pin(1)).unwrap();
    }

    #[test]
    fn a_configured_pin_overrides_a_remembered_one() {
        let dir = tempfile::tempdir().unwrap();
        let pins = PeerPins::load(dir.path(), Vec::new()).unwrap();
        pins.accept("http://peer:3000", &pin(1)).unwrap();

        let configured = vec![("http://peer:3000".to_string(), pin(2))];
        let pins = PeerPins::load(dir.path(), configured).unwrap();
        assert!(pins.accept("http://peer:3000", &pin(1)).is_err());
        pins.accept("http://peer:3000", &pin(2)).unwrap();
    }

    #[test]
    fn pin_arguments_are_parsed_and_validated() {
        let endpoint = hex::encode([7u8; protocol::IROH_ENDPOINT_ID_BYTES]);
        let key = crypto::b64_encode(&[8u8; crypto::ED25519_PUBLIC_KEY_SIZE]);
        let (url, parsed) =
            parse_pin_argument(&format!("http://peer:3000/={endpoint}:{key}")).unwrap();
        assert_eq!(url, "http://peer:3000");
        assert_eq!(parsed.endpoint_id, endpoint);

        assert!(parse_pin_argument("http://peer:3000").is_err());
        assert!(parse_pin_argument(&format!("http://peer:3000=zz:{key}")).is_err());
    }
}
