#![cfg(feature = "kvm-tests")]

use anyhow::{Context, Result, ensure};
use serial_test::serial;
use std::{path::Path, process::Command};

fn run_mesh(forced_relay: bool) -> Result<()> {
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "KVM mesh requires Linux x86_64"
    );
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("integration crate has no workspace parent")?;
    let image = workspace.join("target/kvm/agent.raw");
    let binaries = workspace.join("target/kvm/bin");
    ensure!(
        image.is_file(),
        "missing KVM image {}; run sh tests/kvm/build-image.sh as root",
        image.display()
    );
    for name in [
        "podmesh-agent",
        "podmesh-scheduler",
        "podmesh-proxy",
        "podmesh-sidecar",
        "podmesh-kvm-http",
    ] {
        ensure!(
            binaries.join(name).is_file(),
            "missing KVM guest binary {name}; run sh tests/kvm/build-binaries.sh"
        );
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_kvm-spike"));
    command
        .current_dir(workspace)
        .arg("mesh")
        .arg(image)
        .arg(binaries);
    if forced_relay {
        command.arg("--forced-relay");
    }
    let status = command.status().context("start KVM mesh supervisor")?;
    ensure!(status.success(), "KVM mesh supervisor failed: {status}");
    Ok(())
}

#[test]
#[serial]
fn eight_vm_direct_mesh() -> Result<()> {
    run_mesh(false)
}

#[test]
#[serial]
fn eight_vm_forced_relay_mesh() -> Result<()> {
    run_mesh(true)
}
