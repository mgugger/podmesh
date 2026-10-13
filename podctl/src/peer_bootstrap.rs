//! Owner-mediated initial peer delivery. No endpoint or DNS server is created
//! in podctl; verified records travel through encrypted, target-bound commands.
use super::*;
use protocol::service_discovery::ReplicaServiceRecord;

pub(super) async fn bootstrap(
    key_dir: &Path,
    deployment: &str,
    options: &ClientOptions,
) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(120),
        bootstrap_inner(key_dir, deployment, options),
    )
    .await
    .context("DNS bootstrap timed out; recorded placements remain available for retry")?
}

async fn bootstrap_inner(key_dir: &Path, deployment: &str, options: &ClientOptions) -> Result<()> {
    let catalog = catalog::load(key_dir, deployment)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut records = Vec::new();
    loop {
        records.clear();
        let mut pending = Vec::new();
        for placement in &catalog.replicas {
            match record(key_dir, placement, options.api_base()).await {
                Ok(record) => records.push(record),
                Err(error) => pending.push(format!("replica {}: {error}", placement.replica_index)),
            }
        }
        if pending.is_empty() {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "workloads are deployed but DNS peer bootstrap is pending: {}",
            pending.join("; ")
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // A later workload joins existing tenant peers rather than forming an island.
    let mut existing = Vec::new();
    for other in catalog::load_all(key_dir)?
        .into_iter()
        .filter(|other| other.deployment_id != deployment)
        .take(8)
    {
        for placement in other.replicas.iter().take(2) {
            if let Ok(record) = record(key_dir, placement, options.api_base()).await {
                existing.push(record);
            }
            if existing.len() >= 2 {
                break;
            }
        }
        if existing.len() >= 2 {
            break;
        }
    }
    let mut failures = Vec::new();
    for (index, placement) in catalog.replicas.iter().enumerate() {
        let seeds = select_seeds(&records, &existing, index);
        for seed in seeds {
            match command(
                key_dir,
                placement,
                WorkloadOperation::SeedPeer(Box::new(seed)),
                None,
                options.api_base(),
            )
            .await
            {
                Ok(response) if response.ok => {}
                Ok(_) => failures.push(format!(
                    "replica {} refused peer seed delivery",
                    placement.replica_index
                )),
                Err(error) => failures.push(format!(
                    "replica {} seed delivery: {error}",
                    placement.replica_index
                )),
            }
        }
    }
    // Introduce the new workload in the opposite direction too, so an existing
    // workload can resolve it before waiting for a fresh neighbor connection.
    for other in catalog::load_all(key_dir)?
        .into_iter()
        .filter(|other| other.deployment_id != deployment)
        .take(8)
    {
        for placement in other.replicas.iter().take(2) {
            for seed in records.iter().take(2) {
                if let Err(error) = command(
                    key_dir,
                    placement,
                    WorkloadOperation::SeedPeer(Box::new(seed.clone())),
                    None,
                    options.api_base(),
                )
                .await
                {
                    log::debug!("existing workload introduction unavailable: {error}");
                }
            }
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "DNS peer bootstrap incomplete; placements remain in the catalog for retry: {}",
        failures.join("; ")
    );
    Ok(())
}

pub(super) async fn wait_ready(
    key_dir: &Path,
    placement: &ReplicaPlacement,
    api: Option<&str>,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(record) = record(key_dir, placement, api).await
                && record.catalog.revision_id == placement.revision_id { return Ok(()); }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }).await.context("updated connector did not enroll/publish the requested revision; remaining replicas were not replaced")?
}

async fn record(
    key_dir: &Path,
    placement: &ReplicaPlacement,
    api: Option<&str>,
) -> Result<ReplicaServiceRecord> {
    let response = command(
        key_dir,
        placement,
        WorkloadOperation::ConnectorInspect,
        None,
        api,
    )
    .await?;
    anyhow::ensure!(response.ok, "connector is not ready");
    let report =
        protocol::workload_connector::ConnectorResponse::from_bytes(response.payload.as_bytes())?
            .identity
            .context("connector report missing")?;
    let record = report
        .service_record
        .context("service discovery enrollment is pending")?;
    record.verify(&report.owner_public_key_b64, now_secs())?;
    Ok(record)
}

fn select_seeds(
    records: &[ReplicaServiceRecord],
    existing: &[ReplicaServiceRecord],
    index: usize,
) -> Vec<ReplicaServiceRecord> {
    let mut seeds = Vec::new();
    if !records.is_empty() {
        for offset in [1, (records.len() / 2).max(1)] {
            let other = (index + offset) % records.len();
            if other != index
                && !seeds.iter().any(|seed: &ReplicaServiceRecord| {
                    seed.endpoint.endpoint_id == records[other].endpoint.endpoint_id
                })
            {
                seeds.push(records[other].clone());
            }
        }
    }
    for record in existing.iter().take(2) {
        seeds.push(record.clone());
    }
    seeds.truncate(4);
    seeds
}
