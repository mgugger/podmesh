use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use podmesh_integration_tests::{
    kvm::{KvmRun, MESH_ATTACHMENTS, MESH_GUESTS, command, worker},
    support::ClientKeyDir,
};
use std::collections::HashSet;

struct PhaseTimer {
    started: Instant,
    previous: Instant,
}

impl PhaseTimer {
    fn new() -> Self {
        let started = Instant::now();
        Self {
            started,
            previous: started,
        }
    }

    fn mark(&mut self, phase: &str) {
        let now = Instant::now();
        println!(
            "timing: {phase}: {:.2}s (elapsed {:.2}s)",
            now.duration_since(self.previous).as_secs_f64(),
            now.duration_since(self.started).as_secs_f64()
        );
        self.previous = now;
    }
}

#[derive(serde::Deserialize)]
struct EndpointRecordResponse {
    endpoint_record_b64: String,
}

#[derive(serde::Deserialize)]
struct WorkloadRelayBootstrap {
    auth_token: String,
    ca_certificate_b64: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = env_logger::try_init();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("worker") if args.len() == 7 => worker(
            Path::new(&args[2]),
            Path::new(&args[3]),
            &args[4],
            args[5].parse().context("worker index")?,
            args[6].parse().context("worker memory budget")?,
        ),
        Some("probe") if args.len() == 3 => {
            let mut run = KvmRun::launch(Path::new(&args[2]), &std::env::current_exe()?).await?;
            let result = probe(&run).await;
            let shutdown = run.stop().await;
            result?;
            shutdown?;
            println!("libkrun two-VM kernel/Podman probe passed");
            Ok(())
        }
        Some("run") if args.len() == 4 => {
            let started = Instant::now();
            let mut run = KvmRun::launch(Path::new(&args[2]), &std::env::current_exe()?).await?;
            let result = exercise(&run, Path::new(&args[3])).await;
            if result.is_err() {
                for (index, guest) in run.guests.iter().enumerate() {
                    match run.ssh(index, "for role in scheduler proxy agent; do if [ -f /var/log/podmesh-$role.log ]; then if [ \"$role\" = scheduler ]; then grep -E 'scheduler Iroh endpoint started|machine relay connection denied|Failed to connect to relay|admitted scheduler|dial failed|ERROR|WARN|agent attached' /var/log/podmesh-$role.log | tail -60; else tail -40 /var/log/podmesh-$role.log; fi; fi; done; podman ps -a --format '{{.Names}} {{.Status}}'; for container in $(podman ps -a --format '{{.Names}}' --filter name=podmesh-sidecar); do podman logs --tail 40 \"$container\" 2>&1; done; for container in $(podman ps -a --format '{{.Names}}' --filter name=-app); do podman logs --tail 20 \"$container\" 2>&1; done", None).await {
                        Ok(logs) => eprintln!("{} diagnostics:\n{logs}", guest.name),
                        Err(error) => eprintln!("{} diagnostics failed: {error:#}", guest.name),
                    }
                }
            }
            let shutdown = run.stop().await;
            result?;
            shutdown?;
            println!(
                "libkrun two-VM workload slice passed in {:.1}s",
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Some(mode @ ("mesh" | "two-tier"))
            if (mode == "mesh"
                && (args.len() == 4 || (args.len() == 5 && args[4] == "--forced-relay")))
                || (mode == "two-tier"
                    && (args.len() == 5 || (args.len() == 6 && args[5] == "--forced-relay"))) =>
        {
            let postgres = (mode == "two-tier").then(|| Path::new(&args[4]));
            if let Some(archive) = postgres {
                ensure!(archive.is_file(), "missing PostgreSQL image archive");
            }
            let mut timer = PhaseTimer::new();
            let mut run = if postgres.is_some() {
                KvmRun::launch_two_tier_mesh(Path::new(&args[2]), &std::env::current_exe()?).await?
            } else {
                KvmRun::launch_mesh(Path::new(&args[2]), &std::env::current_exe()?).await?
            };
            timer.mark("eight-VM boot and pinned SSH");
            let result = exercise_mesh(
                &mut run,
                Path::new(&args[3]),
                args.last().is_some_and(|arg| arg == "--forced-relay"),
                postgres,
            )
            .await;
            timer.mark("mesh scenario");
            if result.is_err() {
                for (index, guest) in run.guests.iter().enumerate() {
                    match run.ssh(index, "for role in scheduler proxy agent; do if [ -f /var/log/podmesh-$role.log ]; then tail -40 /var/log/podmesh-$role.log; fi; done; podman ps -a --format '{{.Names}} {{.Status}}'; for container in $(podman ps -a --format '{{.Names}}' --filter name=podmesh-sidecar); do podman logs --tail 40 \"$container\" 2>&1; done; for container in $(podman ps -a --format '{{.Names}}' --filter name=-app); do podman logs --tail 20 \"$container\" 2>&1; done", None).await {
                        Ok(logs) => eprintln!("{} diagnostics:\n{logs}", guest.name),
                        Err(error) => eprintln!("{} diagnostics failed: {error:#}", guest.name),
                    }
                }
            }
            let shutdown = run.stop().await;
            timer.mark("VM shutdown and network cleanup");
            result?;
            shutdown?;
            println!(
                "libkrun eight-VM distributed mesh passed in {:.1}s",
                timer.started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        _ => bail!(
            "usage: kvm-spike probe <guest.raw> | run <guest.raw> <binary-directory> | mesh <guest.raw> <binary-directory> [--forced-relay] | two-tier <guest.raw> <binary-directory> <postgres.tar> [--forced-relay]"
        ),
    }
}

async fn wait_http(client: &reqwest::Client, url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    while Instant::now() < deadline {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => last = response.status().to_string(),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("HTTP readiness deadline at {url}: {last}")
}

async fn wait_for_scheduler_relay_records(
    client: &reqwest::Client,
    scheduler_urls: &[String],
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut missing = Vec::new();
        for url in scheduler_urls {
            let response = client
                .get(format!(
                    "{}/api/v1/endpoint_record",
                    url.trim_end_matches('/')
                ))
                .send()
                .await?
                .error_for_status()?
                .json::<EndpointRecordResponse>()
                .await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let record = protocol::EndpointRecord::from_bytes(
                &crypto::b64_decode(&response.endpoint_record_b64)?,
                now,
            )?;
            if record.relay_url.is_none() {
                missing.push(url);
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "scheduler endpoint records did not advertise relay addresses: {missing:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn exercise(run: &KvmRun, binaries: &Path) -> Result<()> {
    probe(run).await?;
    for (role, guest) in [("scheduler", 0), ("proxy", 0), ("agent", 1), ("sidecar", 1)] {
        let path = binaries.join(format!("podmesh-{role}"));
        run.put(
            guest,
            &format!("/opt/podmesh/podmesh-{role}"),
            &std::fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .await?;
    }
    run.put(
        1,
        "/opt/podmesh/podmesh-kvm-http",
        &std::fs::read(binaries.join("podmesh-kvm-http"))?,
    )
    .await?;
    let control = run.guests[0].address;
    let agent = run.guests[1].address;
    let scheduler_url = format!("http://{control}:3000");
    let proxy_url = format!("http://{control}:3010");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()?;
    let scheduler = format!(
        "#!/bin/sh\nset -eu\nexport RUST_LOG=info,tracing::span=warn,iroh=warn\nexec /opt/podmesh/podmesh-scheduler --listen 0.0.0.0:3000 --machine-key-dir /var/lib/podmesh/scheduler --machine-bind 0.0.0.0:5001 --relay-audience https://{control}:7440 --relay-http-listen 0.0.0.0:7070 --relay-https-listen 0.0.0.0:7440 --relay-qad-listen 0.0.0.0:7840 --relay-metrics-listen 0.0.0.0:9090 --machine-relay-url https://{control}:7440\n"
    );
    run.put(0, "/opt/podmesh/scheduler-command", scheduler.as_bytes())
        .await?;
    wait_http(&client, &format!("{scheduler_url}/health")).await?;
    let public_cert = run
        .ssh(
            0,
            "cat /var/lib/podmesh/scheduler/machine-relay/relay.crt",
            None,
        )
        .await?;
    run.put(1, "/opt/podmesh/machine-relay.crt", public_cert.as_bytes())
        .await?;
    let proxy = format!(
        "#!/bin/sh\nset -eu\nexport RUST_LOG=info,tracing::span=warn,iroh=warn\nexec /opt/podmesh/podmesh-proxy --key-dir /var/lib/podmesh/proxy --iroh-bind 0.0.0.0:4001 --advertise-address {control}:4001 --rest-api-port 3010 --enable-ingress --publish-relay-bootstrap --workload-relay-url https://{control}:7450 --workload-relay-http-listen 0.0.0.0:7080 --workload-relay-https-listen 0.0.0.0:7450 --workload-relay-qad-listen 0.0.0.0:7850 --workload-relay-metrics-listen 0.0.0.0:9100\n"
    );
    run.put(0, "/opt/podmesh/proxy-command", proxy.as_bytes())
        .await?;
    wait_http(&client, &format!("{proxy_url}/healthz")).await?;

    let preload = r#"
    mkdir -p /tmp/sidecar-root/usr/local/bin /tmp/app-root/bin /tmp/app-root/lib /tmp/app-root/www
    cp /opt/podmesh/podmesh-sidecar /tmp/sidecar-root/usr/local/bin/podmesh-sidecar
    tar -C /tmp/sidecar-root -cf /tmp/sidecar.tar .
    podman import --change 'ENTRYPOINT ["/usr/local/bin/podmesh-sidecar"]' /tmp/sidecar.tar localhost/podmesh-kvm/sidecar:spike
    cp /opt/podmesh/podmesh-kvm-http /tmp/app-root/app
    tar -C /tmp/app-root -cf /tmp/app.tar .
    podman import --change 'ENTRYPOINT ["/app"]' /tmp/app.tar localhost/podmesh-kvm/app:spike
    "#;
    run.ssh(1, preload, None)
        .await
        .context("preload local test OCI images")?;
    let agent_command = format!(
        "#!/bin/sh\nset -eu\nexport RUST_LOG=info,tracing::span=warn,iroh=warn\nexec /opt/podmesh/podmesh-agent --key-dir /var/lib/podmesh/agent --state-path /var/lib/podmesh/agent/state.redb --listen 0.0.0.0:3100 --machine-bind 0.0.0.0:5011 --scheduler-url {scheduler_url} --machine-relay-url https://{control}:7440 --machine-relay-ca-certificate /opt/podmesh/machine-relay.crt --sidecar-image localhost/podmesh-kvm/sidecar:spike --capacity-cpu-milli 1000 --capacity-memory-bytes 536870912 --capacity-storage-bytes 3221225472\n"
    );
    run.put(1, "/opt/podmesh/agent-command", agent_command.as_bytes())
        .await?;
    wait_http(&client, &format!("http://{agent}:3100/health")).await?;
    wait_http(
        &client,
        &format!("{scheduler_url}/api/v1/agents/select?cpu_milli=1&memory_bytes=1&storage_bytes=1"),
    )
    .await?;
    let agent_key = run
        .ssh(
            1,
            "base64 /var/lib/podmesh/agent/pubkey.bin | tr -d '\\n'",
            None,
        )
        .await?;
    let owner = ClientKeyDir::with_trusted_agents(&[agent_key])?;
    owner.activate();
    podctl::cert::trust_proxy_async_at(owner.path(), &proxy_url, false).await?;
    let options = podctl::ClientOptions {
        api_base: Some(scheduler_url.clone()),
        trust_any_agent: false,
    };
    let manifest = run.directory().join("workload.yml");
    std::fs::write(&manifest, include_bytes!("../../kvm/workload.yml"))?;
    let id = podctl::apply_file_with_proxy_urls(manifest.clone(), &options, proxy_url).await?;
    let (owner_public, _) = crypto::load_or_create_signing_keypair(owner.path())?;
    let workload = protocol::workload_id(&owner_public, "kvm-app", 0);
    let runtime = podmesh_agent::sidecar::workload_runtime_name(&workload);
    let app = format!("{runtime}-pod-app");
    let sidecar = format!("{runtime}-pod-podmesh-sidecar");
    let app_ip = run
        .ssh(
            1,
            &format!("podman inspect -f '{{{{range .NetworkSettings.Networks}}}}{{{{.IPAddress}}}}{{{{end}}}}' {app}"),
            None,
        )
        .await?;
    ensure!(
        !app_ip.trim().is_empty(),
        "workload container has no guest IP"
    );
    let local_response = run
        .ssh(
            1,
            &format!(
                "curl --fail --silent --show-error --max-time 5 http://{}:8081/",
                app_ip.trim()
            ),
            None,
        )
        .await
        .context("verify application listener inside its Podman container")?;
    ensure!(
        local_response.trim() == "podmesh-kvm-response",
        "unexpected local workload response: {local_response:?}"
    );
    let ingress_url = format!("http://{control}:8080/");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(response) = client
            .get(&ingress_url)
            .header("host", "kvm-app.mesh.local")
            .send()
            .await
            && response.status().is_success()
            && response.text().await?.trim() == "podmesh-kvm-response"
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "ingress response deadline exceeded"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let status = podctl::get_pod(&id, &options).await?;
    ensure!(
        serde_json::from_str::<Vec<serde_json::Value>>(&status)?.len() == 1,
        "status must contain one replica"
    );
    let egress_response = run
        .ssh(
            1,
            &format!(
                "podman exec {sidecar} /usr/local/bin/podmesh-sidecar --help >/dev/null; podman exec {app} /app http://{control}:3000/health"
            ),
            None,
        )
        .await
        .context("transparent workload egress")?;
    ensure!(
        egress_response.trim() == "ok",
        "unexpected transparent workload egress response: {egress_response:?}"
    );
    let logs = run
        .ssh(1, &format!("podman logs --tail 100 {sidecar} 2>&1"), None)
        .await?;
    for evidence in ["Egress connection from", "egress tunnel established"] {
        ensure!(
            logs.contains(evidence),
            "missing sidecar evidence: {evidence}"
        );
    }
    let proxy_logs = run
        .ssh(0, "tail -100 /var/log/podmesh-proxy.log", None)
        .await?;
    ensure!(
        proxy_logs.contains("egress tunnel"),
        "missing proxy egress handling evidence"
    );
    println!("guest-local workload placement, ingress and transparent egress passed");
    let identity_before = run
        .ssh(
            0,
            "base64 /var/lib/podmesh/scheduler/signing/pubkey.bin",
            None,
        )
        .await?;
    let scheduler_binary = binaries.join("podmesh-scheduler");
    let expected_hash = command(
        "sha256sum",
        &[scheduler_binary.to_str().context("scheduler binary path")?],
        None,
    )
    .await?
    .split_whitespace()
    .next()
    .context("scheduler binary sha256 output")?
    .to_string();
    run.put(
        0,
        "/opt/podmesh/podmesh-scheduler",
        &std::fs::read(&scheduler_binary)?,
    )
    .await?;
    let previous_pid = run.ssh(0, "cat /run/podmesh-scheduler.pid", None).await?;
    run.restart_role(0, "scheduler").await?;
    run.ssh(0, &format!("old_pid={}; for attempt in $(seq 1 30); do new_pid=$(cat /run/podmesh-scheduler.pid 2>/dev/null || true); if [ -n \"$new_pid\" ] && [ \"$new_pid\" != \"$old_pid\" ] && kill -0 \"$new_pid\" 2>/dev/null; then exit 0; fi; sleep 1; done; exit 1", previous_pid.trim()), None)
        .await
        .context("wait for scheduler process replacement")?;
    wait_http(
        &client,
        &format!("{scheduler_url}/api/v1/agents/select?cpu_milli=1&memory_bytes=1&storage_bytes=1"),
    )
    .await?;
    ensure!(
        identity_before
            == run
                .ssh(
                    0,
                    "base64 /var/lib/podmesh/scheduler/signing/pubkey.bin",
                    None,
                )
                .await?,
        "scheduler identity changed across replacement"
    );
    let loaded_hash = run
        .ssh(0, "sha256sum /opt/podmesh/podmesh-scheduler", None)
        .await?
        .split_whitespace()
        .next()
        .context("loaded scheduler sha256 output")?
        .to_string();
    ensure!(
        expected_hash == loaded_hash,
        "replacement binary differs from supplied build"
    );
    podctl::get_pod(&id, &options).await?;
    podctl::get_logs(&id, Some(20), &options).await?;
    podctl::delete_file(manifest, false, &options).await?;
    let remaining = run
        .ssh(1, "podman pod ps --format '{{.Name}}'", None)
        .await?;
    ensure!(
        !remaining.contains(&runtime),
        "workload pod remained after delete"
    );
    println!("scheduler binary replacement preserved identity and workload; logs/delete passed");
    Ok(())
}

fn mesh_guest(run: &KvmRun, name: &str) -> Result<usize> {
    let index = MESH_GUESTS
        .iter()
        .position(|guest| *guest == name)
        .context("unknown mesh guest")?;
    ensure!(
        run.guests
            .get(index)
            .is_some_and(|guest| guest.name == name),
        "launched guest list differs from the mesh topology"
    );
    Ok(index)
}

fn mesh_address(run: &KvmRun, name: &str) -> Result<std::net::Ipv4Addr> {
    Ok(run.guests[mesh_guest(run, name)?].address)
}

async fn wait_for_offer(
    client: &reqwest::Client,
    scheduler_url: &str,
    cpu_milli: u32,
) -> Result<protocol::CapacityOffer> {
    let url = format!(
        "{scheduler_url}/api/v1/agents/select?cpu_milli={cpu_milli}&memory_bytes=1&storage_bytes=1"
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_status = String::new();
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                return response
                    .json::<protocol::CapacityOffer>()
                    .await
                    .context("decode signed capacity offer");
            }
            Ok(response) => last_status = response.status().to_string(),
            Err(error) => last_status = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("capacity offer deadline at {url}: {last_status}")
}

async fn wait_proxy_response(
    client: &reqwest::Client,
    proxy_address: std::net::Ipv4Addr,
) -> Result<()> {
    let url = format!("http://{proxy_address}:8080/");
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    while Instant::now() < deadline {
        match client
            .get(&url)
            .header("host", "kvm-app.mesh.local")
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let body = response.text().await?;
                if body.trim() == "podmesh-kvm-response" {
                    return Ok(());
                }
                last = format!("unexpected body: {body:?}");
            }
            Ok(response) => last = response.status().to_string(),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("proxy ingress deadline at {url}: {last}")
}

async fn verify_guest_pods(run: &KvmRun, expected_runtimes: &[String]) -> Result<()> {
    let mut observed = HashSet::new();
    for agent_name in ["agent-1", "agent-2", "agent-3"] {
        let index = mesh_guest(run, agent_name)?;
        let pods = run
            .ssh(index, "podman pod ps --format '{{.Name}}'", None)
            .await?;
        let matching: Vec<_> = pods
            .lines()
            .filter(|pod| {
                expected_runtimes
                    .iter()
                    .any(|runtime| pod.starts_with(runtime))
            })
            .collect();
        ensure!(
            matching.len() == 1,
            "{agent_name} must run exactly one expected workload pod; found {matching:?}"
        );
        let runtime = expected_runtimes
            .iter()
            .find(|runtime| matching[0].starts_with(runtime.as_str()))
            .context("guest pod did not match a deployed runtime")?;
        ensure!(
            observed.insert(runtime.as_str()),
            "workload runtime {runtime} appeared on multiple agent VMs"
        );
    }
    ensure!(
        observed.len() == expected_runtimes.len(),
        "not every replica runtime was observed on its own agent VM"
    );
    Ok(())
}

async fn verify_proxy_egress(
    run: &KvmRun,
    client: &reqwest::Client,
    options: &podctl::ClientOptions,
    owner: &ClientKeyDir,
    proxy_name: &str,
    workload_name: &str,
) -> Result<()> {
    let (owner_public, owner_private) = crypto::load_or_create_signing_keypair(owner.path())?;
    let proxy_url = format!("http://{}:3010", mesh_address(run, proxy_name)?);
    let bootstrap_url = proxy_url.clone();
    let target = format!(
        "{}/health",
        options
            .api_base
            .as_deref()
            .context("missing scheduler URL")?
    );
    let source = std::str::from_utf8(include_bytes!("../../kvm/workload-mesh.yml"))?;
    let egress_manifest = source
        .replace("kvm-app", workload_name)
        .replace("replicas: 3", "replicas: 1");
    let manifest = run.directory().join(format!("{workload_name}-egress.yml"));
    std::fs::write(&manifest, egress_manifest)?;
    let endpoint_record = client
        .get(format!(
            "{}/api/v1/endpoint_record",
            proxy_url.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<EndpointRecordResponse>()
        .await?
        .endpoint_record_b64;
    let bootstrap = client
        .get(format!(
            "{}/api/v1/workload_relay_bootstrap",
            bootstrap_url.trim_end_matches('/')
        ))
        .query(&[("owner", crypto::b64_encode(&owner_public))])
        .send()
        .await?
        .error_for_status()?
        .json::<WorkloadRelayBootstrap>()
        .await?;
    podctl::cert::grant_proxy_async_at(&proxy_url, &owner_public, &owner_private, 30, owner.path())
        .await?;
    podctl::apply_file_with_proxy_endpoints(
        manifest.clone(),
        options,
        vec![endpoint_record],
        bootstrap.auth_token,
        vec![crypto::b64_decode(&bootstrap.ca_certificate_b64)?],
    )
    .await
    .with_context(|| format!("deploy single-proxy workload through {workload_name}"))?;
    let workload_id = protocol::workload_id(&owner_public, workload_name, 0);
    let runtime = podmesh_agent::sidecar::workload_runtime_name(&workload_id);
    let app = format!("{runtime}-pod-app");
    let sidecar = format!("{runtime}-pod-podmesh-sidecar");
    let agent = workload_guest(run, &runtime).await?;
    let readiness_deadline = Instant::now() + Duration::from_secs(60);
    let mut sidecar_logs = String::new();
    while Instant::now() < readiness_deadline {
        sidecar_logs = run
            .ssh(
                agent,
                &format!("podman logs --tail 100 {sidecar} 2>&1"),
                None,
            )
            .await?;
        if sidecar_logs.contains("sidecar registration acknowledged") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    ensure!(
        sidecar_logs.contains("sidecar registration acknowledged"),
        "single-proxy sidecar did not register before egress: {sidecar_logs}"
    );
    let response = run
        .ssh(agent, &format!("podman exec {app} /app {target}"), None)
        .await
        .with_context(|| format!("transparent egress via {workload_name}"))?;
    ensure!(
        response.trim() == "ok",
        "unexpected egress response via {workload_name}: {response:?}"
    );
    let sidecar_logs = run
        .ssh(
            agent,
            &format!("podman logs --tail 100 {sidecar} 2>&1"),
            None,
        )
        .await?;
    ensure!(
        sidecar_logs.contains("egress tunnel established"),
        "missing sidecar tunnel evidence via {workload_name}"
    );
    let proxy_index = mesh_guest(run, proxy_name)?;
    let proxy_logs = run
        .ssh(proxy_index, "tail -100 /var/log/podmesh-proxy.log", None)
        .await?;
    ensure!(
        proxy_logs.contains("egress tunnel"),
        "missing proxy egress handling evidence via {workload_name}"
    );
    podctl::delete_file(manifest, false, options)
        .await
        .with_context(|| format!("delete single-proxy egress workload {workload_name}"))?;
    let remaining = run
        .ssh(agent, "podman pod ps --format '{{.Name}}'", None)
        .await?;
    ensure!(
        !remaining.contains(&runtime),
        "single-proxy egress workload remained after delete"
    );
    Ok(())
}

async fn workload_guest(run: &KvmRun, runtime: &str) -> Result<usize> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        for agent in ["agent-1", "agent-2", "agent-3"] {
            let index = mesh_guest(run, agent)?;
            let pods = run
                .ssh(index, "podman pod ps --format '{{.Name}}'", None)
                .await?;
            if pods.lines().any(|pod| pod == format!("{runtime}-pod")) {
                return Ok(index);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("workload {runtime} never appeared on an agent")
}

async fn wait_workload_http(
    run: &KvmRun,
    guest: usize,
    app: &str,
    url: &str,
    expected: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match run
            .ssh(guest, &format!("podman exec {app} /app {url}"), None)
            .await
        {
            Ok(response) => {
                ensure!(
                    response.trim() == expected,
                    "DNS sample reached an unexpected backend: {response:?}"
                );
                return Ok(());
            }
            Err(error) => {
                ensure!(
                    Instant::now() < deadline,
                    "DNS sample HTTP readiness deadline: {error:#}"
                );
                log::debug!("DNS sample HTTP not ready: {error:#}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn verify_workload_dns(
    run: &KvmRun,
    options: &podctl::ClientOptions,
    owner_public: &[u8],
    proxy_urls: &[String],
    forced_relay: bool,
) -> Result<()> {
    let backend_manifest = run.directory().join("workload-dns-backend.yml");
    let client_manifest = run.directory().join("workload-dns-client.yml");
    fs::write(
        &backend_manifest,
        include_bytes!("../../kvm/workload-dns-backend.yml"),
    )?;
    fs::write(
        &client_manifest,
        include_bytes!("../../kvm/workload-dns-client.yml"),
    )?;
    for manifest in [&backend_manifest, &client_manifest] {
        let id =
            podctl::apply_file_with_proxy_urls(manifest.clone(), options, proxy_urls.join(","))
                .await
                .context("deploy DNS sample workload")?;
        for report in podctl::get_connector_identities(&id, options).await? {
            println!(
                "DNS sample bootstrap {}: current relay={}, published relay={}, published addresses={:?}",
                report.workload_name,
                report.endpoint.relay_url.is_some(),
                report
                    .service_record
                    .as_ref()
                    .is_some_and(|record| record.endpoint.relay_url.is_some()),
                report
                    .service_record
                    .as_ref()
                    .map(|record| &record.endpoint.direct_addresses),
            );
        }
    }
    let backend = podmesh_agent::sidecar::workload_runtime_name(&protocol::workload_id(
        owner_public,
        "dns-backend",
        0,
    ));
    let source = podmesh_agent::sidecar::workload_runtime_name(&protocol::workload_id(
        owner_public,
        "dns-client",
        0,
    ));
    let backend_guest = workload_guest(run, &backend).await?;
    let source_guest = workload_guest(run, &source).await?;
    ensure!(
        backend_guest != source_guest,
        "DNS sample client/backend must occupy different agent VMs"
    );
    let backend_app = format!("{backend}-pod-app");
    let source_app = format!("{source}-pod-app");
    let hostname = run
        .ssh(
            backend_guest,
            &format!("podman inspect {backend_app} --format '{{{{.Config.Hostname}}}}'"),
            None,
        )
        .await?;
    ensure!(
        hostname.trim() == format!("{backend}-pod"),
        "backend runtime did not use its own pod hostname"
    );
    let expected = format!("podmesh-kvm-response:{}", hostname.trim());
    wait_workload_http(
        run,
        backend_guest,
        &backend_app,
        "http://127.0.0.1:8081/",
        &expected,
    )
    .await?;
    wait_workload_http(
        run,
        source_guest,
        &source_app,
        "http://127.0.0.1:8082/",
        "podmesh-kvm-response",
    )
    .await?;
    ensure!(
        run.ssh(
            source_guest,
            &format!("podman exec {source_app} /app http://127.0.0.1:8081/"),
            None,
        )
        .await
        .is_err(),
        "DNS client unexpectedly serves the backend target port"
    );
    let fqdn = podmesh_sidecar::service_cache::ServiceKey {
        manifest_id: protocol::route_id(owner_public, "dns-backend"),
        name: "echo".into(),
    }
    .fqdn()?;
    let url = format!("http://{fqdn}:18081/");
    let deadline = Instant::now() + Duration::from_secs(45);
    let resolved = loop {
        match run
            .ssh(
                source_guest,
                &format!("podman exec {source_app} /app resolve {fqdn}:18081"),
                None,
            )
            .await
        {
            Ok(resolved) => break resolved,
            Err(error) => {
                ensure!(
                    Instant::now() < deadline,
                    "DNS sample resolution deadline: {error:#}"
                );
                log::debug!("DNS sample resolution not ready: {error:#}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    };
    let addresses = resolved
        .lines()
        .map(str::parse::<std::net::SocketAddr>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        !addresses.is_empty()
            && addresses.iter().all(|address| {
                matches!(address.ip(), std::net::IpAddr::V4(ip) if ip.octets()[..3] == [198, 18, 0])
                    && address.port() == 18081
            }),
        "DNS sample did not resolve exclusively to synthetic service VIPs: {resolved:?}"
    );
    println!(
        "DNS sample resolved {fqdn} to {}; client={} backend={}",
        addresses[0], run.guests[source_guest].name, run.guests[backend_guest].name,
    );
    wait_workload_http(run, source_guest, &source_app, &url, expected.trim()).await?;
    if forced_relay {
        run.block_relays(true).await?;
        let blocked = run
            .ssh(
                source_guest,
                &format!("podman exec {source_app} /app {url}"),
                None,
            )
            .await;
        run.block_relays(false).await?;
        ensure!(
            blocked.is_err(),
            "cross-VM DNS service traffic bypassed the blocked relay path"
        );
        wait_workload_http(run, source_guest, &source_app, &url, expected.trim()).await?;
    }
    println!(
        "DNS sample: {} -> {} via {fqdn}:18081 -> 8081, VIP={}, backend={}",
        run.guests[source_guest].name,
        run.guests[backend_guest].name,
        addresses[0].ip(),
        expected.trim()
    );
    for manifest in [client_manifest, backend_manifest] {
        podctl::delete_file(manifest, false, options)
            .await
            .context("delete DNS sample workload")?;
    }
    for guest in [source_guest, backend_guest] {
        let pods = run
            .ssh(guest, "podman pod ps --format '{{.Name}}'", None)
            .await?;
        ensure!(
            !pods
                .lines()
                .any(|pod| pod == format!("{source}-pod") || pod == format!("{backend}-pod")),
            "DNS sample pod remained after deletion"
        );
    }
    Ok(())
}

async fn verify_two_tier(
    run: &KvmRun,
    options: &podctl::ClientOptions,
    owner: &ClientKeyDir,
    client: &reqwest::Client,
    proxy_urls: &[String],
    postgres_image: &Path,
    forced_relay: bool,
) -> Result<()> {
    let archive = fs::read(postgres_image).context("read PostgreSQL image archive")?;
    for agent in ["agent-1", "agent-2", "agent-3"] {
        run.ssh(
            mesh_guest(run, agent)?,
            "cat > /tmp/postgres.tar; podman load -i /tmp/postgres.tar; rm /tmp/postgres.tar; podman image exists docker.io/library/postgres:16-alpine",
            Some(&archive),
        )
        .await
        .with_context(|| format!("preload PostgreSQL image on {agent}"))?;
    }
    let db_manifest = run.directory().join("demo_two_tier_db.yml");
    let frontend_manifest = run.directory().join("demo_two_tier_frontend.yml");
    fs::write(
        &db_manifest,
        include_bytes!("../../../deploy/demo_two_tier_db.yml"),
    )?;
    fs::write(
        &frontend_manifest,
        include_bytes!("../../../deploy/demo_two_tier_frontend.yml"),
    )?;
    let database =
        podctl::apply_file_with_proxy_urls(db_manifest.clone(), options, proxy_urls.join(","))
            .await
            .context("deploy two-tier PostgreSQL")?;
    let frontend = podctl::apply_file_with_proxy_urls(
        frontend_manifest.clone(),
        options,
        proxy_urls.join(","),
    )
    .await
    .context("deploy two-tier frontend")?;
    let db_catalog = podctl::catalog::load(owner.path(), &database)?;
    let frontend_catalog = podctl::catalog::load(owner.path(), &frontend)?;
    ensure!(
        db_catalog.replicas.len() == 1
            && frontend_catalog.replicas.len() == 1
            && db_catalog.replicas[0].agent_endpoint_id
                != frontend_catalog.replicas[0].agent_endpoint_id,
        "two-tier anti-affinity did not select distinct agents"
    );
    let db_runtime =
        podmesh_agent::sidecar::workload_runtime_name(&db_catalog.replicas[0].receipt.workload_id);
    let frontend_runtime = podmesh_agent::sidecar::workload_runtime_name(
        &frontend_catalog.replicas[0].receipt.workload_id,
    );
    let db_guest = workload_guest(run, &db_runtime).await?;
    let frontend_guest = workload_guest(run, &frontend_runtime).await?;
    ensure!(
        db_guest != frontend_guest,
        "two-tier pods co-located on one VM"
    );
    let frontend_app = format!("{frontend_runtime}-pod-frontend");
    run.ssh(
        frontend_guest,
        &format!("podman cp /opt/podmesh/podmesh-kvm-http {frontend_app}:/tmp/podmesh-kvm-http"),
        None,
    )
    .await?;
    let resolved = run
        .ssh(
            frontend_guest,
            &format!(
                "podman exec {frontend_app} /tmp/podmesh-kvm-http resolve postgres-db.svc.podmesh.internal:5432"
            ),
            None,
        )
        .await?;
    let addresses = resolved
        .lines()
        .map(str::parse::<std::net::SocketAddr>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        !addresses.is_empty()
            && addresses.iter().all(|address| {
                matches!(address.ip(), std::net::IpAddr::V4(ip) if ip.octets()[..3] == [198, 18, 0])
                    && address.port() == 5432
            }),
        "two-tier database alias did not resolve exclusively to synthetic VIPs: {resolved:?}"
    );
    let url = format!("http://{}:8080/", mesh_address(run, "proxy-1")?);
    let wait_ingress = || async {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let pending = match client
                .get(&url)
                .header("Host", "demo-frontend.mesh.local")
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await?;
                    if status.is_success()
                        && body.trim() == "DB_RESULT=podmesh-two-tier-postgres-ok"
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    format!("status={status}, body={body:?}")
                }
                Err(error) => error.to_string(),
            };
            ensure!(
                Instant::now() < deadline,
                "two-tier SQL ingress readiness deadline: {pending}"
            );
            eprintln!("two-tier SQL ingress pending: {pending}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    wait_ingress().await?;
    let query = format!(
        r#"podman exec --env PGCONNECT_TIMEOUT=5 {frontend_app} sh -c 'exec psql -h "$DB_HOST" -p "$DB_PORT" -tAc "$1"' sh "SELECT 'podmesh-two-tier-postgres-ok';""#
    );
    if forced_relay {
        run.block_relays(true).await?;
        let blocked = run.ssh(frontend_guest, &query, None).await;
        run.block_relays(false).await?;
        ensure!(
            blocked.is_err(),
            "fresh two-tier SQL query bypassed blocked relay transport"
        );
        wait_ingress().await?;
    }
    let result = run.ssh(frontend_guest, &query, None).await?;
    ensure!(
        result.trim() == "podmesh-two-tier-postgres-ok",
        "fresh two-tier SQL query returned an unexpected result"
    );
    println!(
        "two-tier VM: frontend={} database={}, DNS={}, ingress DB_RESULT=podmesh-two-tier-postgres-ok",
        run.guests[frontend_guest].name, run.guests[db_guest].name, addresses[0]
    );
    for manifest in [frontend_manifest, db_manifest] {
        podctl::delete_file(manifest, false, options)
            .await
            .context("delete two-tier workload")?;
    }
    for guest in [frontend_guest, db_guest] {
        let pods = run
            .ssh(guest, "podman pod ps --format '{{.Name}}'", None)
            .await?;
        ensure!(
            !pods.lines().any(|pod| {
                pod == format!("{frontend_runtime}-pod") || pod == format!("{db_runtime}-pod")
            }),
            "two-tier pod remained after acknowledged deletion"
        );
    }
    Ok(())
}

async fn exercise_mesh(
    run: &mut KvmRun,
    binaries: &Path,
    forced_relay: bool,
    postgres_image: Option<&Path>,
) -> Result<()> {
    let mut timer = PhaseTimer::new();
    ensure!(
        run.guests.len() == MESH_GUESTS.len(),
        "full mesh requires exactly eight guests"
    );
    probe(run).await?;
    timer.mark("kernel and Podman probes");

    if forced_relay {
        run.force_relay().await?;
        run.allow_agent_bootstrap(true).await?;
    }
    for scheduler in ["scheduler-1", "scheduler-2", "scheduler-3"] {
        run.put(
            mesh_guest(run, scheduler)?,
            "/opt/podmesh/podmesh-scheduler",
            &std::fs::read(binaries.join("podmesh-scheduler"))?,
        )
        .await?;
    }
    for agent in ["agent-1", "agent-2", "agent-3"] {
        let index = mesh_guest(run, agent)?;
        for role in ["agent", "sidecar"] {
            run.put(
                index,
                &format!("/opt/podmesh/podmesh-{role}"),
                &std::fs::read(binaries.join(format!("podmesh-{role}")))?,
            )
            .await?;
        }
        run.put(
            index,
            "/opt/podmesh/podmesh-kvm-http",
            &std::fs::read(binaries.join("podmesh-kvm-http"))?,
        )
        .await?;
    }
    for proxy in ["proxy-1", "proxy-2"] {
        run.put(
            mesh_guest(run, proxy)?,
            "/opt/podmesh/podmesh-proxy",
            &std::fs::read(binaries.join("podmesh-proxy"))?,
        )
        .await?;
    }
    timer.mark("binary injection");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()?;
    let scheduler_ips = ["scheduler-1", "scheduler-2", "scheduler-3"]
        .map(|name| mesh_address(run, name))
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let scheduler_urls = scheduler_ips
        .iter()
        .map(|address| format!("http://{address}:3000"))
        .collect::<Vec<_>>();
    let peer_urls = scheduler_urls.join(",");
    let relay_urls = scheduler_ips
        .iter()
        .map(|address| format!("https://{address}:7440"))
        .collect::<Vec<_>>()
        .join(",");
    let cert_paths = (1..=3)
        .map(|index| format!("/opt/podmesh/machine-relay-{index}.crt"))
        .collect::<Vec<_>>()
        .join(",");
    let relay_cert_paths = cert_paths.clone();

    // Relay certificates are generated on the host so every scheduler trusts its peers from first start.
    for (number, scheduler) in ["scheduler-1", "scheduler-2", "scheduler-3"]
        .into_iter()
        .enumerate()
        .map(|(i, s)| (i + 1, s))
    {
        let address = mesh_address(run, scheduler)?;
        let material = iroh_support::ensure_relay_tls(
            &run.directory().join("relay-certs").join(scheduler),
            &format!("https://{address}:7440"),
            None,
            None,
        )?;
        let cert = fs::read(&material.certificate_path)?;
        let key = fs::read(&material.private_key_path)?;
        for consumer in [
            "scheduler-1",
            "scheduler-2",
            "scheduler-3",
            "agent-1",
            "agent-2",
            "agent-3",
        ] {
            run.put(
                mesh_guest(run, consumer)?,
                &format!("/opt/podmesh/machine-relay-{number}.crt"),
                &cert,
            )
            .await?;
        }
        run.put(
            mesh_guest(run, scheduler)?,
            &format!("/opt/podmesh/relay-key-{number}.pem"),
            &key,
        )
        .await?;
    }
    timer.mark("relay certificate distribution");

    for (number, scheduler) in ["scheduler-1", "scheduler-2", "scheduler-3"]
        .into_iter()
        .enumerate()
        .map(|(i, s)| (i + 1, s))
    {
        let address = mesh_address(run, scheduler)?;
        let index = mesh_guest(run, scheduler)?;
        let command = format!(
            "#!/bin/sh\nset -eu\nd=/var/lib/podmesh/{scheduler}/machine-relay\nmkdir -p $d\nchmod 700 $d\ncp /opt/podmesh/machine-relay-{number}.crt $d/relay.crt\ncp /opt/podmesh/relay-key-{number}.pem $d/relay.key\nchmod 600 $d/relay.key\nexport RUST_LOG=info,tracing::span=warn,iroh=warn,podmesh_scheduler::machine::discovery=debug\nexec /opt/podmesh/podmesh-scheduler --listen 0.0.0.0:3000 --machine-key-dir /var/lib/podmesh/{scheduler} --machine-bind 0.0.0.0:5001 --relay-audience https://{address}:7440 --relay-http-listen 0.0.0.0:7070 --relay-https-listen 0.0.0.0:7440 --relay-qad-listen 0.0.0.0:7840 --relay-metrics-listen 0.0.0.0:9090 --machine-relay-url {relay_urls} --machine-relay-ca-certificate {relay_cert_paths} --scheduler-peer-url {peer_urls}\n"
        );
        run.put(index, "/opt/podmesh/scheduler-command", command.as_bytes())
            .await?;
    }
    for scheduler in ["scheduler-1", "scheduler-2", "scheduler-3"] {
        let address = mesh_address(run, scheduler)?;
        wait_http(&client, &format!("http://{address}:3000/health")).await?;
    }
    timer.mark("scheduler start");
    wait_for_scheduler_relay_records(&client, &scheduler_urls).await?;
    timer.mark("scheduler relay endpoint readiness");

    for proxy in ["proxy-1", "proxy-2"] {
        let address = mesh_address(run, proxy)?;
        let index = mesh_guest(run, proxy)?;
        let bootstrap = if proxy == "proxy-1" {
            "--publish-relay-bootstrap".to_string()
        } else {
            format!(
                "--publish-relay-bootstrap --workload-relay-bootstrap-url http://{}:3010",
                mesh_address(run, "proxy-1")?
            )
        };
        let command = format!(
            "#!/bin/sh\nset -eu\nexport RUST_LOG=info,tracing::span=warn,iroh=warn\nexec /opt/podmesh/podmesh-proxy --key-dir /var/lib/podmesh/{proxy} --iroh-bind 0.0.0.0:4001 --advertise-address {address}:4001 --rest-api-port 3010 --enable-ingress {bootstrap} --workload-relay-url https://{address}:7450 --workload-relay-http-listen 0.0.0.0:7080 --workload-relay-https-listen 0.0.0.0:7450 --workload-relay-qad-listen 0.0.0.0:7850 --workload-relay-metrics-listen 0.0.0.0:9100\n"
        );
        run.put(index, "/opt/podmesh/proxy-command", command.as_bytes())
            .await?;
        wait_http(&client, &format!("http://{address}:3010/healthz")).await?;
    }
    timer.mark("proxy bootstrap");

    let preload = r#"
    mkdir -p /tmp/sidecar-root/usr/local/bin /tmp/app-root
    cp /opt/podmesh/podmesh-sidecar /tmp/sidecar-root/usr/local/bin/podmesh-sidecar
    tar -C /tmp/sidecar-root -cf /tmp/sidecar.tar .
    podman import --change 'ENTRYPOINT ["/usr/local/bin/podmesh-sidecar"]' /tmp/sidecar.tar localhost/podmesh-kvm/sidecar:spike
    cp /opt/podmesh/podmesh-kvm-http /tmp/app-root/app
    tar -C /tmp/app-root -cf /tmp/app.tar .
    podman import --change 'ENTRYPOINT ["/app"]' /tmp/app.tar localhost/podmesh-kvm/app:spike
    "#;
    for agent in ["agent-1", "agent-2", "agent-3"] {
        run.ssh(mesh_guest(run, agent)?, preload, None)
            .await
            .with_context(|| format!("preload OCI images on {agent}"))?;
    }
    timer.mark("three guest-local OCI preloads");

    for (scheduler, agents) in MESH_ATTACHMENTS {
        ensure!(
            agents.len()
                == match scheduler {
                    "scheduler-1" => 0,
                    "scheduler-2" => 2,
                    "scheduler-3" => 1,
                    _ => unreachable!(),
                },
            "configured mesh attachment count differs from the approved topology"
        );
    }
    let (capacity_cpu, capacity_memory) = if postgres_image.is_some() {
        (2000, 1_073_741_824u64)
    } else {
        (1000, 536_870_912u64)
    };
    for agent in ["agent-1", "agent-2", "agent-3"] {
        let scheduler = MESH_ATTACHMENTS
            .iter()
            .find(|(_, agents)| agents.contains(&agent))
            .map(|(scheduler, _)| *scheduler)
            .context("agent is not attached by the approved mesh topology")?;
        let index = mesh_guest(run, agent)?;
        let command = format!(
            "#!/bin/sh\nset -eu\nexport RUST_LOG=info,tracing::span=warn,iroh=warn\nexec /opt/podmesh/podmesh-agent --key-dir /var/lib/podmesh/{agent} --state-path /var/lib/podmesh/{agent}/state.redb --listen 0.0.0.0:3100 --machine-bind 0.0.0.0:5011 --scheduler-url http://{}:3000 --machine-relay-url {relay_urls} --machine-relay-ca-certificate {cert_paths} --sidecar-image localhost/podmesh-kvm/sidecar:spike --capacity-cpu-milli {capacity_cpu} --capacity-memory-bytes {capacity_memory} --capacity-storage-bytes 4294967296\n",
            mesh_address(run, scheduler)?
        );
        run.put(index, "/opt/podmesh/agent-command", command.as_bytes())
            .await?;
        wait_http(
            &client,
            &format!("http://{}:3100/health", mesh_address(run, agent)?),
        )
        .await?;
    }

    wait_for_offer(&client, &scheduler_urls[0], 100).await?;
    let impossible = client
        .get(format!(
            "{}/api/v1/agents/select?cpu_milli=3000&memory_bytes=1&storage_bytes=1",
            scheduler_urls[0]
        ))
        .send()
        .await?;
    ensure!(
        !impossible.status().is_success(),
        "mesh offered capacity for a request above every agent's configured CPU bound"
    );
    timer.mark("agent startup and capacity checks");
    if forced_relay {
        // Agents receive authenticated relay grants over their initial attachment.
        for agent in ["agent-1", "agent-2", "agent-3"] {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let logs = run
                    .ssh(
                        mesh_guest(run, agent)?,
                        "tail -100 /var/log/podmesh-agent.log",
                        None,
                    )
                    .await?;
                if logs.contains("relay address advertised: true") {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "{agent} did not obtain relay grants"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        run.allow_agent_bootstrap(false).await?;
        wait_for_offer(&client, &scheduler_urls[0], 100).await?;
        let (relay_packets, direct_drops) = run.forced_relay_path_counters().await?;
        println!(
            "forced-relay path counters after capacity readiness: relay-TCP={relay_packets}, direct-UDP-dropped={direct_drops}"
        );
        timer.mark("forced-relay capacity readiness after grant bootstrap");
    }

    let mut trusted_agent_keys = Vec::with_capacity(3);
    for agent in ["agent-1", "agent-2", "agent-3"] {
        trusted_agent_keys.push(
            run.ssh(
                mesh_guest(run, agent)?,
                &format!("base64 /var/lib/podmesh/{agent}/pubkey.bin | tr -d '\\n'"),
                None,
            )
            .await?,
        );
    }
    let owner = ClientKeyDir::with_trusted_agents(&trusted_agent_keys)?;
    owner.activate();
    let (owner_public, _) = crypto::load_or_create_signing_keypair(owner.path())?;
    let proxy_urls = ["proxy-1", "proxy-2"]
        .iter()
        .map(|proxy| Ok(format!("http://{}:3010", mesh_address(run, proxy)?)))
        .collect::<Result<Vec<_>>>()?;
    for proxy_url in &proxy_urls {
        podctl::cert::trust_proxy_async_at(owner.path(), proxy_url, false).await?;
    }
    timer.mark("owner agent and proxy trust");
    let options = podctl::ClientOptions {
        api_base: Some(scheduler_urls[0].clone()),
        trust_any_agent: false,
    };
    if forced_relay {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let discovery = podctl::discover_workloads(&options).await?;
            if discovery.unreachable_agents.is_empty()
                && discovery.unreachable_schedulers.is_empty()
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "forced-relay reconciliation remains partial: {discovery:?}"
            );
            eprintln!("relay reconciliation pending: {discovery:?}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        timer.mark("forced-relay reconciliation readiness");
    }
    if let Some(archive) = postgres_image {
        verify_two_tier(
            run,
            &options,
            &owner,
            &client,
            &proxy_urls,
            archive,
            forced_relay,
        )
        .await
        .context("real two-tier VM sample")?;
        if forced_relay {
            ensure!(
                run.forced_relay_dropped_packets().await? > 0,
                "two-tier forced-relay filter observed no direct UDP attempts"
            );
        }
        timer.mark("real PostgreSQL/frontend VM sample");
        return Ok(());
    }
    let manifest = run.directory().join("workload-mesh.yml");
    std::fs::write(&manifest, include_bytes!("../../kvm/workload-mesh.yml"))?;
    let deployment_id =
        podctl::apply_file_with_proxy_urls(manifest.clone(), &options, proxy_urls.join(","))
            .await
            .context("three-replica placement through scheduler-1")?;
    let workload_ids = (0..3)
        .map(|index| protocol::workload_id(&owner_public, "kvm-app", index))
        .collect::<Vec<_>>();
    let runtimes = workload_ids
        .iter()
        .map(|id| podmesh_agent::sidecar::workload_runtime_name(id))
        .collect::<Vec<_>>();
    let catalog = podctl::catalog::load(owner.path(), &deployment_id)?;
    ensure!(
        catalog.replicas.len() == 3
            && catalog
                .replicas
                .iter()
                .map(|replica| replica.agent_endpoint_id.as_str())
                .collect::<HashSet<_>>()
                .len()
                == 3,
        "three replicas were not placed on distinct agent identities"
    );
    verify_guest_pods(run, &runtimes).await?;
    timer.mark("three-replica apply and placement verification");

    for proxy in ["proxy-1", "proxy-2"] {
        wait_proxy_response(&client, mesh_address(run, proxy)?).await?;
    }
    if forced_relay {
        ensure!(
            run.forced_relay_dropped_packets().await? > 0,
            "direct UDP filter did not drop packets"
        );
        run.block_relays(true).await?;
        let blocked = client
            .get(format!("http://{}:8080/", mesh_address(run, "proxy-1")?))
            .header("Host", "kvm-app.mesh.local")
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        run.block_relays(false).await?;
        ensure!(
            !matches!(blocked, Ok(response) if response.status().is_success()),
            "ingress bypassed the blocked relay path"
        );
        wait_proxy_response(&client, mesh_address(run, "proxy-1")?).await?;
        timer.mark("relay-negative control and restored ingress");
    }
    timer.mark("both proxy ingress paths");
    verify_workload_dns(run, &options, &owner_public, &proxy_urls, forced_relay)
        .await
        .context("cross-VM workload DNS sample")?;
    timer.mark("cross-VM workload DNS discovery and TCP");
    for (proxy_name, name) in [("proxy-1", "egress-p1"), ("proxy-2", "egress-p2")] {
        verify_proxy_egress(run, &client, &options, &owner, proxy_name, name).await?;
        timer.mark(name);
    }
    let status = podctl::get_pod(&deployment_id, &options).await?;
    ensure!(
        serde_json::from_str::<Vec<serde_json::Value>>(&status)?.len() == 3,
        "status through scheduler-1 must include all three replicas"
    );
    let logs = podctl::get_logs(&deployment_id, Some(20), &options).await?;
    ensure!(!logs.trim().is_empty(), "bounded workload logs are empty");
    let alternate = podctl::ClientOptions {
        api_base: Some(scheduler_urls[1].clone()),
        trust_any_agent: false,
    };
    let alternate_status = podctl::get_pod(&deployment_id, &alternate).await?;
    ensure!(
        serde_json::from_str::<Vec<serde_json::Value>>(&alternate_status)?.len() == 3,
        "status through scheduler-2 must include all three replicas"
    );
    let discovery = podctl::discover_workloads(&alternate).await?;
    ensure!(
        discovery.unreachable_agents.is_empty()
            && discovery.unreachable_schedulers.is_empty()
            && discovery.workloads.len() == 3,
        "mesh rediscovery through scheduler-2 is incomplete: {discovery:?}"
    );
    timer.mark("status, logs, alternate scheduler and rediscovery");

    let update_manifest = run.directory().join("workload-mesh-update.yml");
    let updated = std::fs::read_to_string(&manifest)?.replacen(
        "      labels:\n        run: kvm-app",
        "      annotations:\n        podmesh.io/test-revision: \"2\"\n      labels:\n        run: kvm-app",
        1,
    );
    std::fs::write(&update_manifest, updated)?;
    podctl::apply_file_with_proxy_urls(update_manifest.clone(), &options, proxy_urls.join(","))
        .await
        .context("fixed-placement three-replica update")?;
    let updated_status = podctl::get_pod(&deployment_id, &options).await?;
    ensure!(
        serde_json::from_str::<Vec<serde_json::Value>>(&updated_status)?.len() == 3,
        "updated status must retain all three replicas"
    );
    verify_guest_pods(run, &runtimes).await?;
    timer.mark("fixed-placement update");

    let identity_before = run
        .ssh(
            mesh_guest(run, "scheduler-1")?,
            "base64 /var/lib/podmesh/scheduler-1/signing/pubkey.bin",
            None,
        )
        .await?;
    let scheduler_binary = binaries.join("podmesh-scheduler");
    run.put(
        mesh_guest(run, "scheduler-1")?,
        "/opt/podmesh/podmesh-scheduler",
        &std::fs::read(&scheduler_binary)?,
    )
    .await?;
    run.restart_role(mesh_guest(run, "scheduler-1")?, "scheduler")
        .await?;
    wait_http(&client, &format!("{}/health", scheduler_urls[0])).await?;
    ensure!(
        identity_before
            == run
                .ssh(
                    mesh_guest(run, "scheduler-1")?,
                    "base64 /var/lib/podmesh/scheduler-1/signing/pubkey.bin",
                    None,
                )
                .await?,
        "scheduler-1 identity changed across restart"
    );
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let pending = match podctl::get_pod(&deployment_id, &options).await {
                Ok(status) => {
                    let count = serde_json::from_str::<Vec<serde_json::Value>>(&status)?.len();
                    if count == 3 {
                        return Ok::<_, anyhow::Error>(());
                    }
                    format!("recovered {count} of 3 replicas")
                }
                Err(error) => format!("{error:#}"),
            };
            eprintln!("scheduler-1 mesh recovery pending: {pending}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("scheduler-1 did not recover all replica locations after restart")??;
    timer.mark("scheduler-1 replacement and recovery");

    for name in [
        "scheduler-1",
        "scheduler-2",
        "scheduler-3",
        "agent-1",
        "agent-2",
        "agent-3",
    ] {
        let role = if name.starts_with("scheduler-") {
            "scheduler"
        } else {
            "agent"
        };
        run.ssh(
            mesh_guest(run, name)?,
            &format!("kill -STOP \"$(cat /run/podmesh-{role}.pid)\""),
            None,
        )
        .await?;
    }
    for proxy in ["proxy-1", "proxy-2"] {
        wait_proxy_response(&client, mesh_address(run, proxy)?).await?;
    }
    for name in [
        "scheduler-1",
        "scheduler-2",
        "scheduler-3",
        "agent-1",
        "agent-2",
        "agent-3",
    ] {
        let role = if name.starts_with("scheduler-") {
            "scheduler"
        } else {
            "agent"
        };
        run.ssh(
            mesh_guest(run, name)?,
            &format!("kill -CONT \"$(cat /run/podmesh-{role}.pid)\""),
            None,
        )
        .await?;
    }

    timer.mark("traffic with control processes stopped and resumed");
    podctl::delete_file(update_manifest, false, &options).await?;
    for agent in ["agent-1", "agent-2", "agent-3"] {
        let pods = run
            .ssh(
                mesh_guest(run, agent)?,
                "podman pod ps --format '{{.Name}}'",
                None,
            )
            .await?;
        ensure!(
            !runtimes.iter().any(|runtime| pods.contains(runtime)),
            "workload runtime remained on {agent} after delete: {pods}"
        );
    }
    timer.mark("delete and guest cleanup verification");
    if forced_relay {
        let forced_udp_drops = run.forced_relay_dropped_packets().await?;
        ensure!(
            forced_udp_drops > 0,
            "forced-relay filter observed no denied direct UDP attempts"
        );
        println!("forced-relay direct UDP drop counter: {forced_udp_drops}");
    }
    println!(
        "three schedulers, distinct-agent replicas, both proxies, update, restart and delete passed"
    );
    Ok(())
}

async fn probe(run: &KvmRun) -> Result<()> {
    for (index, guest) in run.guests.iter().enumerate() {
        let output = run.ssh(index, "uname -r; awk '$2 == \"/\" {print $1, $3}' /proc/mounts; podman info --format '{{.Host.CgroupsVersion}} {{.Store.GraphDriverName}}'; test -d /sys/fs/cgroup; unshare -n true; nft add table ip podmesh_probe; nft 'add chain ip podmesh_probe output { type nat hook output priority -100; }'; nft add rule ip podmesh_probe output tcp dport 12345 dnat to 127.0.0.1:15001; nft delete table ip podmesh_probe", None).await?;
        ensure!(
            output.contains("v2 overlay"),
            "guest Podman is not using cgroup v2/overlay: {output}"
        );
        println!(
            "{} ({}) kernel/Podman:\n{output}",
            guest.name, guest.address
        );
    }
    println!(
        "private disks allocated: {} bytes",
        run.allocated_disk_bytes()?
    );
    Ok(())
}
