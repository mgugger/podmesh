use anyhow::{Context, Result, anyhow};

pub const DEFAULT_SIDECAR_CPU_MILLI: u32 = 100;
pub const DEFAULT_SIDECAR_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
pub const DEFAULT_SIDECAR_STORAGE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestResources {
    pub cpu_milli: u32,
    pub memory_bytes: u64,
    pub storage_bytes: u64,
}

impl ManifestResources {
    pub fn with_default_sidecar(self) -> Result<Self> {
        Ok(Self {
            cpu_milli: self
                .cpu_milli
                .checked_add(DEFAULT_SIDECAR_CPU_MILLI)
                .ok_or_else(|| anyhow!("CPU limit overflow after sidecar injection"))?,
            memory_bytes: self
                .memory_bytes
                .checked_add(DEFAULT_SIDECAR_MEMORY_BYTES)
                .ok_or_else(|| anyhow!("memory limit overflow after sidecar injection"))?,
            storage_bytes: self
                .storage_bytes
                .checked_add(DEFAULT_SIDECAR_STORAGE_BYTES)
                .ok_or_else(|| anyhow!("storage limit overflow after sidecar injection"))?,
        })
    }
}

pub fn validate_and_measure_manifest(manifest: &[u8]) -> Result<(Vec<u8>, ManifestResources)> {
    let manifest = std::str::from_utf8(manifest).context("workload manifest must be UTF-8")?;
    let mutated = crate::manifest_policy::validate_and_mutate_manifest(manifest)?;
    let documents = crate::manifest_yaml::parse_yaml_documents_from_str(&mutated)?;
    let mut cpu_milli = 0u64;
    let mut memory_bytes = 0u64;
    let mut storage_bytes = 0u64;
    let mut container_count = 0usize;

    for document in &documents {
        let Some(containers) = containers(document) else {
            continue;
        };
        for container in containers {
            container_count = container_count.saturating_add(1);
            let limits = container
                .get("resources")
                .and_then(|resources| resources.get("limits"))
                .ok_or_else(|| anyhow!("container resource limits are required"))?;
            cpu_milli = cpu_milli
                .checked_add(u64::from(parse_cpu(required_quantity(limits, "cpu")?)?))
                .ok_or_else(|| anyhow!("aggregate CPU limit overflow"))?;
            memory_bytes = memory_bytes
                .checked_add(parse_bytes(required_quantity(limits, "memory")?)?)
                .ok_or_else(|| anyhow!("aggregate memory limit overflow"))?;
            storage_bytes = storage_bytes
                .checked_add(parse_bytes(required_quantity(
                    limits,
                    "ephemeral-storage",
                )?)?)
                .ok_or_else(|| anyhow!("aggregate storage limit overflow"))?;
        }
    }
    anyhow::ensure!(container_count > 0, "workload contains no containers");
    let cpu_milli = u32::try_from(cpu_milli).map_err(|_| anyhow!("aggregate CPU exceeds u32"))?;
    Ok((
        mutated.into_bytes(),
        ManifestResources {
            cpu_milli,
            memory_bytes,
            storage_bytes,
        },
    ))
}

fn containers(document: &serde_yaml::Value) -> Option<&Vec<serde_yaml::Value>> {
    let kind = document.get("kind").and_then(serde_yaml::Value::as_str)?;
    let spec = if kind == "Pod" {
        document.get("spec")?
    } else {
        document.get("spec")?.get("template")?.get("spec")?
    };
    spec.get("containers")?.as_sequence()
}

fn required_quantity<'a>(limits: &'a serde_yaml::Value, key: &str) -> Result<&'a str> {
    limits
        .get(key)
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| anyhow!("resource limit {key} must be a string quantity"))
}

fn parse_cpu(quantity: &str) -> Result<u32> {
    if let Some(milli) = quantity.strip_suffix('m') {
        let value = milli.parse::<u32>().context("invalid millicore quantity")?;
        anyhow::ensure!(value > 0, "CPU limit must be greater than zero");
        return Ok(value);
    }
    let (whole, fraction) = quantity.split_once('.').unwrap_or((quantity, ""));
    anyhow::ensure!(
        !whole.is_empty()
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.len() <= 3
            && fraction.bytes().all(|byte| byte.is_ascii_digit()),
        "unsupported CPU quantity"
    );
    let whole = whole.parse::<u32>().context("invalid CPU quantity")?;
    let mut fractional = fraction.to_string();
    while fractional.len() < 3 {
        fractional.push('0');
    }
    let fractional = if fractional.is_empty() {
        0
    } else {
        fractional.parse::<u32>().context("invalid CPU fraction")?
    };
    let milli = whole
        .checked_mul(1000)
        .and_then(|value| value.checked_add(fractional))
        .ok_or_else(|| anyhow!("CPU quantity overflow"))?;
    anyhow::ensure!(milli > 0, "CPU limit must be greater than zero");
    Ok(milli)
}

fn parse_bytes(quantity: &str) -> Result<u64> {
    const SUFFIXES: [(&str, u128); 8] = [
        ("Ti", 1024u128.pow(4)),
        ("Gi", 1024u128.pow(3)),
        ("Mi", 1024u128.pow(2)),
        ("Ki", 1024),
        ("T", 1000u128.pow(4)),
        ("G", 1000u128.pow(3)),
        ("M", 1000u128.pow(2)),
        ("K", 1000),
    ];
    let (number, multiplier) = SUFFIXES
        .iter()
        .find_map(|(suffix, multiplier)| {
            quantity
                .strip_suffix(suffix)
                .map(|number| (number, *multiplier))
        })
        .unwrap_or((quantity, 1));
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    anyhow::ensure!(
        !whole.is_empty()
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.len() <= 9
            && fraction.bytes().all(|byte| byte.is_ascii_digit()),
        "unsupported byte quantity"
    );
    let scale = 10u128.pow(fraction.len() as u32);
    let whole = whole.parse::<u128>().context("invalid byte quantity")?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u128>()
            .context("invalid byte quantity fraction")?
    };
    let scaled = whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fraction))
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| anyhow!("byte quantity overflow"))?;
    let bytes = scaled
        .checked_add(scale.saturating_sub(1))
        .ok_or_else(|| anyhow!("byte quantity rounding overflow"))?
        / scale;
    anyhow::ensure!(bytes > 0, "byte limit must be greater than zero");
    u64::try_from(bytes).map_err(|_| anyhow!("byte quantity exceeds u64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_quantities() {
        assert_eq!(parse_cpu("500m").unwrap(), 500);
        assert_eq!(parse_cpu("2").unwrap(), 2_000);
        assert_eq!(parse_cpu("0.125").unwrap(), 125);
        assert!(parse_cpu("100u").is_err());
    }

    #[test]
    fn parses_fractional_byte_quantities_without_undercounting() {
        assert_eq!(parse_bytes("1.5Gi").unwrap(), 1536 * 1024 * 1024);
        assert_eq!(parse_bytes("0.1K").unwrap(), 100);
        assert!(parse_bytes("1e3").is_err());
    }

    #[test]
    fn measures_defaults_for_all_containers() {
        let manifest = br#"kind: Pod
metadata: { name: demo }
spec:
  containers:
    - { name: one, image: nginx }
    - { name: two, image: nginx }
"#;
        let (_, resources) = validate_and_measure_manifest(manifest).unwrap();
        assert_eq!(resources.cpu_milli, 200);
        assert_eq!(resources.memory_bytes, 256 * 1024 * 1024);
        assert_eq!(resources.storage_bytes, 2 * 1024 * 1024 * 1024);
    }
}
