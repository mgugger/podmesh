use anyhow::{Context, Result, ensure};

const README: &str = include_str!("../../README.md");
const DEPLOY_README: &str = include_str!("../../deploy/README.md");
const SCOPE: &str = include_str!("../../docs/poc-scope.md");
const AGENT_SPEC: &str = include_str!("../../openspec/specs/agent/spec.md");
const LOCAL_SPEC: &str = include_str!("../../openspec/specs/local-deployment/spec.md");
const MESSAGE_SPEC: &str = include_str!("../../openspec/specs/message-security/spec.md");
const PODCTL_SPEC: &str = include_str!("../../openspec/specs/podctl-cli/spec.md");
const SCHEDULER_SPEC: &str = include_str!("../../openspec/specs/scheduler/spec.md");
const TRAFFIC_SPEC: &str = include_str!("../../openspec/specs/workload-traffic-plane/spec.md");
const DECISIONS: &str = include_str!("../../openspec/decisions.md");
const DEVELOPMENT_SPEC: &str = include_str!("../../openspec/specs/development-workflow/spec.md");

#[test]
fn greenfield_scope_uses_normal_tests_without_specialized_assurance() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for removed in [
        "fuzz",
        "tools/release-evidence",
        "scripts/release-evidence",
        "release/policy-waivers.json",
        "tests/src/release_evidence.rs",
        "tests/src/bin/release_harness.rs",
        "tests/tests/release_baseline.rs",
        "tests/tests/release_soak.rs",
        "tests/tests/release_partitions.rs",
        "tests/tests/release_documentation.rs",
        "docs/mvp-release-contract.md",
        "openspec/specs/release-assurance",
        ".github/workflows/extended-evidence.yml",
        ".github/workflows/release-evidence.yml",
        ".github/release-tool-versions.env",
    ] {
        ensure!(
            !root.join(removed).exists(),
            "removed assurance surface still exists: {removed}"
        );
    }
    let instructions = include_str!("../../.github/copilot-instructions.md");
    for required in [
        "## Greenfield PoC Focus",
        "single Pod/Deployment with fixed replicas",
        "trust boundary",
        "ordinary unit, integration, adversarial",
        "do not add",
        "removed scope",
        "Do not recreate them",
    ] {
        ensure!(
            instructions
                .to_ascii_lowercase()
                .contains(&required.to_ascii_lowercase()),
            "missing greenfield instruction: {required}"
        );
    }
    ensure!(DEVELOPMENT_SPEC.contains("Greenfield work SHALL prioritize the Podmesh PoC"));
    let workspace: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../openspec/config.yaml"))?;
    ensure!(
        workspace["context"]
            .as_str()
            .context("missing context")?
            .contains("greenfield")
    );
    for manifest in [
        include_str!("../../Cargo.toml"),
        include_str!("../Cargo.toml"),
    ] {
        ensure!(!manifest.contains("release-evidence"));
    }
    ensure!(!include_str!("complete_rootless_stack.rs").contains("SOAK"));
    ensure!(include_str!("../Cargo.toml").contains("proptest"));
    Ok(())
}

#[test]
fn instructions_keep_adaptive_behavior_and_create_openspec_artifacts() -> Result<()> {
    let instructions = include_str!("../../.github/copilot-instructions.md");
    for required in [
        "## Adaptive Workflow",
        "scope/risk",
        "structured clarifying questions",
        "On resume",
        "openspec/changes/<change>/proposal.md",
        "design.md",
        "tasks.md",
        "specs/<capability>/spec.md",
        "openspec/changes/<change>/verification.md",
        "openspec new change <change>",
        "for approval",
        "counts as approval",
        "Small, clear fixes",
        "acceptance is distinct",
        "Do not recreate",
    ] {
        ensure!(
            instructions.contains(required),
            "missing workflow guidance: {required}"
        );
    }
    for entry in [
        include_str!("../../.github/skills/openspec-propose/SKILL.md"),
        include_str!("../../.github/skills/openspec-apply-change/SKILL.md"),
        include_str!("../../.github/prompts/opsx-propose.prompt.md"),
        include_str!("../../.github/prompts/opsx-apply.prompt.md"),
    ] {
        ensure!(entry.contains("openspec/README.md") && entry.contains("approval"));
        ensure!(entry.contains("design") && entry.contains("tasks"));
    }
    for requirement in [
        "Adaptive collaboration SHALL produce OpenSpec artifacts",
        "Session continuity and review SHALL use the change record",
        "An already presented plan is approved",
        "A small clear fix is requested",
        "Implementation is complete but not accepted",
    ] {
        ensure!(
            DEVELOPMENT_SPEC.contains(requirement),
            "missing workflow scenario: {requirement}"
        );
    }
    Ok(())
}

#[test]
fn active_workflow_uses_openspec_without_parallel_configuration() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for path in [
        "openspec/README.md",
        "openspec/decisions.md",
        "openspec/code-inventory.md",
        "openspec/specs/development-workflow/spec.md",
    ] {
        ensure!(
            root.join(path).is_file(),
            "missing migration destination: {path}"
        );
    }
    ensure!(
        !root.join("aidlc-docs").exists(),
        "retired AI-DLC tree must not be recreated"
    );
    ensure!(!root.join(".aidlc-rule-details").exists());
    ensure!(!root.join(".github/chatmodes/Plan.chatmode.md").exists());
    let instructions = include_str!("../../.github/copilot-instructions.md");
    ensure!(instructions.contains("OpenSpec is the sole active"));
    ensure!(!instructions.contains("ALWAYS follow this workflow FIRST"));
    ensure!(instructions.lines().count() < 100);
    for (path, expected_tools) in [
        (".github/agents/Plan.agent.md", vec!["read", "search"]),
        (
            ".github/prompts/check-dry-violations.prompt.md",
            vec!["read", "search", "edit", "execute"],
        ),
    ] {
        let document = std::fs::read_to_string(root.join(path))?;
        let frontmatter = document
            .strip_prefix("---\n")
            .context("missing frontmatter")?
            .split_once("\n---")
            .context("unterminated frontmatter")?
            .0;
        let config: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
        ensure!(
            config["description"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let tools: Vec<_> = config["tools"]
            .as_sequence()
            .context("missing tools")?
            .iter()
            .filter_map(serde_yaml::Value::as_str)
            .collect();
        ensure!(tools == expected_tools, "unexpected tools in {path}");
        ensure!(document.contains("openspec/README.md"));
    }
    Ok(())
}

#[test]
fn executable_sources_do_not_load_retired_workflow_records() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut directories: Vec<_> = [
        "podctl",
        "podmesh-agent",
        "podmesh-proxy",
        "podmesh-scheduler",
        "podmesh-sidecar",
        "shared",
        "tests",
    ]
    .into_iter()
    .map(|path| root.join(path))
    .collect();
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.push(path);
            } else if matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("rs" | "py" | "sh")
            ) && path != root.join("tests/tests/project_documentation.rs")
            {
                let source = std::fs::read_to_string(&path)?;
                ensure!(
                    !source.contains("aidlc-docs/") && !source.contains(".aidlc-rule-details/"),
                    "executable source still depends on legacy workflow: {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn openspec_configuration_declares_one_workflow_and_valid_artifact_rules() -> Result<()> {
    let config: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../openspec/config.yaml"))?;
    ensure!(config["schema"].as_str() == Some("spec-driven"));
    let context = config["context"]
        .as_str()
        .context("missing OpenSpec context")?;
    ensure!(context.contains("sole active source"));
    for required in [
        "adaptive planning",
        "structured questions/answers",
        "approval checkpoints",
        "verification/review",
        "session continuity",
        "proposal.md",
        "design.md",
        "tasks.md",
        "verification.md",
        "already presented plan",
        "Small clear fixes",
    ] {
        ensure!(
            context.contains(required),
            "missing adaptive artifact context: {required}"
        );
    }
    ensure!(!context.contains("files under 300 lines"));
    for artifact in ["proposal", "design", "specs", "tasks"] {
        ensure!(config["rules"][artifact].as_sequence().is_some());
    }
    Ok(())
}

#[test]
fn current_project_guides_link_to_existing_openspec_records() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for relative in [
        "openspec/README.md",
        "openspec/decisions.md",
        ".github/copilot-instructions.md",
    ] {
        let path = root.join(relative);
        let document = std::fs::read_to_string(&path)?;
        for suffix in document.split("](").skip(1) {
            let target = suffix.split(')').next().context("missing link target")?;
            if target.starts_with('#') || target.contains("://") {
                continue;
            }
            let target_path = target.split('#').next().unwrap();
            ensure!(
                !target_path.contains("aidlc-docs"),
                "{relative} links to removed legacy content"
            );
            ensure!(
                path.parent().unwrap().join(target_path).exists(),
                "{relative} has a missing local link: {target}"
            );
        }
    }
    Ok(())
}

#[test]
fn poc_guarantees_do_not_reintroduce_reviewed_overclaims() -> Result<()> {
    for (name, document) in documents()
        .into_iter()
        .chain(std::iter::once(("OpenSpec decisions", DECISIONS)))
        .chain([("development workflow", DEVELOPMENT_SPEC)])
    {
        let normalized = document
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for forbidden in [
            "cannot reach the host or another tenant",
            "could reach the host or another tenant",
            "shall not learn the replica count",
            "shall not learn deployment replica count",
            "proxies are not trusted with workload plaintext",
            "needs the tenant's private key",
            "network topology, which cannot be a boundary",
            "bridge would not survive",
        ] {
            ensure!(
                !normalized.contains(forbidden),
                "{name} reintroduces an unscoped guarantee: {forbidden}"
            );
        }
    }
    for required in [
        "**End-to-end encryption:**",
        "**Statelessness:**",
        "**Zero trust:**",
        "**Identity-based authorization:**",
        "**Decentralization:**",
        "**Replay detection:**",
        "## Failure Recovery",
        "cleanup-required",
        "operator repair",
    ] {
        ensure!(
            SCOPE.contains(required),
            "PoC scope is missing the reviewed boundary: {required}"
        );
    }
    Ok(())
}

#[test]
fn ordinary_checks_and_poc_limits_are_consistent() -> Result<()> {
    for command in [
        "cargo test --quiet --locked --workspace",
        "cargo clippy --locked --workspace --all-targets -- -D warnings",
    ] {
        ensure!(README.contains(command), "root README is missing {command}");
    }
    for (name, document) in documents() {
        let normalized = document.to_ascii_lowercase();
        for forbidden in [
            "podmesh is production ready",
            "backward compatibility is guaranteed",
            "mixed-version operation is supported",
            "automatic agent-loss recovery is guaranteed",
            "ingress tls is provided",
            "tenant network isolation is provided",
        ] {
            ensure!(
                !normalized.contains(forbidden),
                "{name} contains an unsupported guarantee: {forbidden}"
            );
        }
    }
    for required in [
        "greenfield",
        "mixed-version",
        "shared networking",
        "plain http",
        "bearer workload",
        "bootstrap",
    ] {
        ensure!(
            SCOPE.to_ascii_lowercase().contains(required),
            "PoC scope is missing limitation: {required}"
        );
    }
    ensure!(
        DEPLOY_README.contains("PODMESH_IMAGE_TAG=poc-local-1"),
        "deploy guide is missing the optional image-tag command"
    );
    Ok(())
}

fn documents() -> [(&'static str, &'static str); 9] {
    [
        ("root README", README),
        ("deploy README", DEPLOY_README),
        ("PoC scope", SCOPE),
        ("agent spec", AGENT_SPEC),
        ("local deployment spec", LOCAL_SPEC),
        ("message security spec", MESSAGE_SPEC),
        ("podctl spec", PODCTL_SPEC),
        ("scheduler spec", SCHEDULER_SPEC),
        ("traffic spec", TRAFFIC_SPEC),
    ]
}
