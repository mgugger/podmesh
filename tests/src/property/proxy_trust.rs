use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

pub fn signing_key(seed: u8) -> String {
    crypto::b64_encode(&[seed; crypto::ED25519_PUBLIC_KEY_SIZE])
}

pub fn endpoint_id(seed: u8) -> String {
    hex::encode([seed; protocol::IROH_ENDPOINT_ID_BYTES])
}

pub fn identity(label: u8, endpoint_seed: u8, key_seed: u8) -> podctl::trust::TrustedProxyIdentity {
    let origin = podctl::trust::CanonicalProxyOrigin::parse(&format!(
        "http://proxy-{label}.example:{}",
        7000 + u16::from(label)
    ))
    .unwrap();
    podctl::trust::TrustedProxyIdentity::new(
        origin,
        &endpoint_id(endpoint_seed),
        &signing_key(key_seed),
    )
    .unwrap()
}

pub fn identity_strategy() -> impl Strategy<Value = podctl::trust::TrustedProxyIdentity> {
    (any::<u8>(), any::<u8>(), any::<u8>())
        .prop_map(|(label, endpoint, key)| identity(label, endpoint, key))
}

pub fn registry_strategy() -> impl Strategy<Value = podctl::trust::TrustRegistry> {
    (
        proptest::collection::btree_set(any::<u8>(), 0..16),
        proptest::collection::btree_map(0u8..32, (any::<u8>(), any::<u8>()), 0..8),
    )
        .prop_map(|(agents, proxies)| registry_from_seeds(&agents, &proxies))
}

pub fn registry_from_seeds(
    agents: &BTreeSet<u8>,
    proxies: &BTreeMap<u8, (u8, u8)>,
) -> podctl::trust::TrustRegistry {
    let mut document = String::from(podctl::trust::TRUST_FILE_HEADER);
    document.push('\n');
    for seed in agents {
        document.push_str("agent ");
        document.push_str(&signing_key(*seed));
        document.push('\n');
    }
    for (label, (endpoint, key)) in proxies {
        let entry = identity(*label, *endpoint, *key);
        document.push_str(&format!(
            "proxy {} {} {}\n",
            entry.origin, entry.endpoint_id_hex, entry.signing_key_b64
        ));
    }
    podctl::trust::TrustRegistry::parse(&document).unwrap()
}
