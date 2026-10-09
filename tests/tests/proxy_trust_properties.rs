use std::collections::{BTreeMap, BTreeSet};

use podmesh_integration_tests::property::proxy_trust::{
    endpoint_id, identity, identity_strategy, registry_strategy, signing_key,
};
use podmesh_integration_tests::property::proxy_trust_model::{
    apply, command_strategy, commit_model,
};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

const U01_PROPTEST_SEED: u64 = 0x504f_444d_4553_4801;

fn u01_proptest_config() -> ProptestConfig {
    eprintln!("U-01 proptest fixed seed: {U01_PROPTEST_SEED}");
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(U01_PROPTEST_SEED),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(u01_proptest_config())]

    #[test]
    fn tp01_typed_store_round_trip(registry in registry_strategy()) {
        let encoded = registry.to_canonical_string();
        prop_assert_eq!(podctl::trust::TrustRegistry::parse(&encoded).unwrap(), registry);
    }

    #[test]
    fn tp02_legacy_migration_preserves_unique_agent_keys(seeds in proptest::collection::vec(any::<u8>(), 0..32)) {
        let mut legacy = String::new();
        for seed in &seeds {
            legacy.push_str(&format!("{} # generated agent\n", signing_key(*seed)));
        }
        let registry = podctl::trust::TrustRegistry::parse(&legacy).unwrap();
        let expected: BTreeSet<_> = seeds.into_iter().map(signing_key).collect();
        prop_assert_eq!(registry.agent_keys(), &expected);
        prop_assert!(registry.proxies().is_empty());
    }

    #[test]
    fn tp03_url_normalization_is_idempotent(label in 0u16..1000, https in any::<bool>()) {
        let scheme = if https { "HTTPS" } else { "HTTP" };
        let input = format!("{scheme}://PROXY-{label}.EXAMPLE/");
        let once = podctl::trust::normalize_proxy_url(&input).unwrap();
        let twice = podctl::trust::normalize_proxy_url(&once).unwrap();
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn tp04_equivalent_origins_normalize_equally(label in 0u16..1000, https in any::<bool>()) {
        let (scheme, default_port) = if https { ("https", 443) } else { ("http", 80) };
        let plain = format!("{scheme}://proxy-{label}.example");
        let explicit = format!("{}://PROXY-{label}.EXAMPLE:{default_port}/", scheme.to_uppercase());
        prop_assert_eq!(
            podctl::trust::normalize_proxy_url(&plain).unwrap(),
            podctl::trust::normalize_proxy_url(&explicit).unwrap()
        );
    }

    #[test]
    fn tp05_canonical_serialization_reaches_a_fixed_point(registry in registry_strategy()) {
        let first = registry.to_canonical_string();
        let second = podctl::trust::TrustRegistry::parse(&first).unwrap().to_canonical_string();
        prop_assert_eq!(first, second);
    }

    #[test]
    fn tp06_authorization_requires_exact_identity(identity in identity_strategy(), replacement_seed in any::<u8>()) {
        let mut registry = podctl::trust::TrustRegistry::default();
        registry.insert_proxy(identity.clone()).unwrap();
        prop_assert!(registry.authorize_proxy(&identity).is_ok());
        let different = podctl::trust::TrustedProxyIdentity::new(
            identity.origin.clone(),
            &endpoint_id(replacement_seed.wrapping_add(1)),
            &identity.signing_key_b64,
        ).unwrap();
        if different.endpoint_id_hex != identity.endpoint_id_hex {
            prop_assert!(registry.authorize_proxy(&different).is_err());
        }
    }

    #[test]
    fn tp07_agent_override_never_changes_proxy_state(
        registry in registry_strategy(),
        environment_seeds in proptest::collection::btree_set(any::<u8>(), 0..16),
    ) {
        let before = registry.to_canonical_string();
        let override_value = environment_seeds.iter().copied().map(signing_key).collect::<Vec<_>>().join(",");
        let effective = registry.effective_agent_keys(Some(&override_value)).unwrap();
        let expected: BTreeSet<_> = environment_seeds.into_iter().map(signing_key).collect();
        prop_assert_eq!(effective, expected);
        prop_assert_eq!(registry.to_canonical_string(), before);
    }

    #[test]
    fn tp08_proxy_capacity_refusal_preserves_state(extra_seed in any::<u8>()) {
        let mut registry = podctl::trust::TrustRegistry::default();
        for label in 0..podctl::trust::MAX_TRUSTED_PROXIES as u8 {
            registry.insert_proxy(identity(label, label, label)).unwrap();
        }
        let before = registry.clone();
        prop_assert!(registry.insert_proxy(identity(200, extra_seed, extra_seed)).is_err());
        prop_assert_eq!(registry, before);
    }

    #[test]
    fn tp09_mutation_sequences_match_reference_model(
        commands in proptest::collection::vec(command_strategy(), 0..64)
    ) {
        let mut registry = podctl::trust::TrustRegistry::default();
        let mut model = BTreeMap::new();
        for command in commands {
            apply(&mut registry, &mut model, command);
        }
    }

    #[test]
    fn tp10_failed_commit_exposes_complete_old_or_new(
        old in proptest::collection::vec(any::<u8>(), 0..256),
        new in proptest::collection::vec(any::<u8>(), 0..256),
        fail_after_rename in any::<bool>(),
    ) {
        let (success, visible) = commit_model(&old, &new, fail_after_rename);
        prop_assert!(!success);
        prop_assert!(visible == old || visible == new);
    }

    #[test]
    fn tp11_remove_is_idempotent(identity in identity_strategy()) {
        let mut registry = podctl::trust::TrustRegistry::default();
        registry.insert_proxy(identity.clone()).unwrap();
        prop_assert!(registry.remove_proxy(&identity.origin).is_some());
        prop_assert!(registry.remove_proxy(&identity.origin).is_none());
        prop_assert!(!registry.proxies().contains_key(&identity.origin));
    }

    #[test]
    fn tp12_distinct_insertions_commute(first_label in 0u8..16, second_label in 16u8..32) {
        let first = identity(first_label, first_label, first_label);
        let second = identity(second_label, second_label, second_label);
        let mut left = podctl::trust::TrustRegistry::default();
        left.insert_proxy(first.clone()).unwrap();
        left.insert_proxy(second.clone()).unwrap();
        let mut right = podctl::trust::TrustRegistry::default();
        right.insert_proxy(second).unwrap();
        right.insert_proxy(first).unwrap();
        prop_assert_eq!(left.to_canonical_string(), right.to_canonical_string());
    }

    #[test]
    fn tp13_rejected_observation_does_not_mutate_registry(identity in identity_strategy()) {
        let mut registry = podctl::trust::TrustRegistry::default();
        registry.insert_proxy(identity.clone()).unwrap();
        let before = registry.clone();
        let mismatched = podctl::trust::TrustedProxyIdentity::new(
            identity.origin.clone(),
            &endpoint_id(identity.endpoint_id_hex.as_bytes()[0].wrapping_add(1)),
            &signing_key(identity.signing_key_b64.as_bytes()[0].wrapping_add(1)),
        ).unwrap();
        let _ = registry.authorize_proxy(&mismatched);
        prop_assert_eq!(registry, before);
    }

}
