use std::collections::BTreeMap;

use proptest::prelude::*;

#[derive(Clone, Debug)]
pub enum TrustCommand {
    Insert(podctl::trust::TrustedProxyIdentity),
    Replace(podctl::trust::TrustedProxyIdentity),
    Remove(podctl::trust::CanonicalProxyOrigin),
}

pub fn command_strategy() -> impl Strategy<Value = TrustCommand> {
    prop_oneof![
        super::proxy_trust::identity_strategy().prop_map(TrustCommand::Insert),
        super::proxy_trust::identity_strategy().prop_map(TrustCommand::Replace),
        (0u8..32).prop_map(|label| TrustCommand::Remove(
            podctl::trust::CanonicalProxyOrigin::parse(&format!(
                "http://proxy-{label}.example:{}",
                7000 + u16::from(label)
            ))
            .unwrap()
        )),
    ]
}

pub fn apply(
    registry: &mut podctl::trust::TrustRegistry,
    model: &mut BTreeMap<podctl::trust::CanonicalProxyOrigin, podctl::trust::TrustedProxyIdentity>,
    command: TrustCommand,
) {
    match command {
        TrustCommand::Insert(identity) => {
            let expected_success = !model.contains_key(&identity.origin)
                && model.len() < podctl::trust::MAX_TRUSTED_PROXIES;
            assert_eq!(
                registry.insert_proxy(identity.clone()).is_ok(),
                expected_success
            );
            if expected_success {
                model.insert(identity.origin.clone(), identity);
            }
        }
        TrustCommand::Replace(identity) => {
            let expected_success = model.contains_key(&identity.origin);
            assert_eq!(
                registry.replace_proxy(identity.clone()).is_ok(),
                expected_success
            );
            if expected_success {
                model.insert(identity.origin.clone(), identity);
            }
        }
        TrustCommand::Remove(origin) => {
            let actual = registry.remove_proxy(&origin);
            let expected = model.remove(&origin);
            assert_eq!(actual, expected);
        }
    }
    assert_eq!(registry.proxies(), model);
}

pub fn commit_model(old: &[u8], new: &[u8], fail_after_rename: bool) -> (bool, Vec<u8>) {
    if fail_after_rename {
        (false, new.to_vec())
    } else {
        (false, old.to_vec())
    }
}
