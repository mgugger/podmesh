use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};

const CI: &str = include_str!("../../.github/workflows/ci.yml");
const VERSIONS: &str = include_str!("../../.github/ci-tool-versions.env");

fn pins() -> BTreeMap<&'static str, &'static str> {
    VERSIONS
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect()
}

#[test]
fn workflow_yaml_parses_and_every_external_action_uses_a_reviewed_sha() -> Result<()> {
    let pins = pins();
    let expected = [
        pins["ACTION_CHECKOUT_SHA"],
        pins["ACTION_RUST_TOOLCHAIN_SHA"],
        pins["ACTION_CACHE_SHA"],
    ];
    let workflow: serde_yaml::Value = serde_yaml::from_str(CI)?;
    for (_, job) in workflow["jobs"].as_mapping().context("missing jobs")? {
        for step in job["steps"].as_sequence().context("missing steps")? {
            let Some(action) = step["uses"].as_str() else {
                continue;
            };
            let (_, revision) = action.rsplit_once('@').context("action is not pinned")?;
            ensure!(
                revision.len() == 40
                    && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && expected.contains(&revision),
                "unreviewed action revision {revision}"
            );
            if action.starts_with("dtolnay/rust-toolchain@") {
                ensure!(step["with"]["toolchain"].as_str() == Some(pins["RUST_TOOLCHAIN_VERSION"]));
            }
        }
    }
    Ok(())
}

#[test]
fn ci_runs_normal_checks_with_read_only_permissions_and_finite_timeouts() -> Result<()> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(CI)?;
    ensure!(parsed["permissions"]["contents"].as_str() == Some("read"));
    for (job_name, job) in parsed["jobs"].as_mapping().context("missing jobs")? {
        ensure!(
            job["timeout-minutes"]
                .as_u64()
                .is_some_and(|value| value > 0 && value <= 90),
            "job {job_name:?} has no finite timeout"
        );
        ensure!(
            job.get("permissions").is_none(),
            "jobs must not widen default permissions"
        );
    }
    ensure!(CI.contains("cargo test --quiet --locked --workspace"));
    ensure!(CI.contains("cargo clippy --workspace --all-targets --locked -- -D warnings"));
    ensure!(CI.contains("cargo deny check"));
    ensure!(CI.contains("--features podman-tests --tests"));
    for removed in [
        "evidence",
        "fuzz",
        "soak",
        "sbom",
        "syft",
        "upload-artifact",
        "download-artifact",
    ] {
        ensure!(
            !CI.to_ascii_lowercase().contains(removed),
            "CI still contains {removed}"
        );
    }
    Ok(())
}

#[test]
fn real_podman_execution_is_explicitly_requested() -> Result<()> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(CI)?;
    let input = &parsed["on"]["workflow_dispatch"]["inputs"]["run_podman"];
    ensure!(input["type"].as_str() == Some("boolean"));
    ensure!(input["default"].as_bool() == Some(false));
    ensure!(
        parsed["jobs"]["podman-tests"]["if"].as_str()
            == Some("github.event_name == 'workflow_dispatch' && inputs.run_podman")
    );
    Ok(())
}

#[test]
fn workflow_files_exist_at_the_only_approved_locations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    assert!(root.join(".github/workflows/ci.yml").is_file());
    assert_eq!(
        fs::read_dir(root.join(".github/workflows"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "yml")
            })
            .count(),
        1
    );
}
