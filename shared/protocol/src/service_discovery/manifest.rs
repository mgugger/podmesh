//! Compile the public Service subset without copying secret workload settings.
use super::{
    MAX_CATALOG_SERVICES, MAX_SERVICE_PORTS, ServiceDefinition, ServicePort, validate_service_label,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

pub fn extract_services(manifest: &[u8]) -> Result<Vec<ServiceDefinition>> {
    let docs = crate::manifest_yaml::parse_yaml_documents_from_slice(manifest)?;
    let docs = docs
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let pod = docs
        .iter()
        .find(|doc| {
            doc.get("kind").and_then(Value::as_str) == Some("Pod")
                || doc.pointer("/spec/template/spec").is_some()
        })
        .context("service manifest has no workload pod")?;
    let spec = pod
        .pointer("/spec/template/spec")
        .or_else(|| pod.get("spec"))
        .context("workload spec missing")?;
    let labels = pod
        .pointer("/spec/template/metadata/labels")
        .or_else(|| pod.pointer("/metadata/labels"));
    let mut named_ports = HashMap::new();
    for container in spec["containers"]
        .as_array()
        .context("workload containers missing")?
    {
        for port in container
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = port.get("name").and_then(Value::as_str) {
                let number = number(&port["containerPort"])?;
                if let Some(old) = named_ports.insert(name.to_owned(), number) {
                    ensure!(old == number, "ambiguous named target port");
                }
            }
        }
    }
    let mut services = BTreeMap::new();
    for doc in &docs {
        if doc.get("kind").and_then(Value::as_str) != Some("Service") {
            continue;
        }
        let name = doc
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .context("Service name missing")?;
        validate_service_label(name)?;
        let spec = doc.get("spec").context("Service spec missing")?;
        ensure!(
            spec.get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "ClusterIP"),
            "internal DNS supports only ClusterIP Services"
        );
        let selectors = spec
            .get("selector")
            .and_then(Value::as_object)
            .context("Service selector missing")?;
        ensure!(
            !selectors.is_empty()
                && selectors
                    .iter()
                    .all(|(key, value)| labels.and_then(|labels| labels.get(key)) == Some(value)),
            "Service selector does not match this workload"
        );
        let mut ports = BTreeMap::new();
        for port in spec["ports"].as_array().context("Service ports missing")? {
            ensure!(
                port.get("protocol")
                    .and_then(Value::as_str)
                    .is_none_or(|protocol| protocol == "TCP"),
                "internal services currently support TCP only"
            );
            let published = number(&port["port"])?;
            let target_port = match port.get("targetPort") {
                None => published,
                Some(Value::String(name)) => *named_ports
                    .get(name)
                    .context("named Service targetPort not declared")?,
                Some(value) => number(value)?,
            };
            ensure!(
                ports
                    .insert(
                        published,
                        ServicePort {
                            port: published,
                            target_port
                        }
                    )
                    .is_none(),
                "duplicate Service port"
            );
        }
        ensure!(
            !ports.is_empty() && ports.len() <= MAX_SERVICE_PORTS,
            "invalid Service port count"
        );
        ensure!(
            services
                .insert(
                    name.to_owned(),
                    ServiceDefinition {
                        name: name.into(),
                        ports: ports.into_values().collect()
                    }
                )
                .is_none(),
            "duplicate Service name"
        );
    }
    ensure!(services.len() <= MAX_CATALOG_SERVICES, "too many Services");
    Ok(services.into_values().collect())
}
/// Every enrolled workload participates in peer networking, including clients
/// without a Kubernetes Service. Its canonical workload endpoint uses the
/// declared application ports, or the existing sidecar app-port convention.
pub fn workload_services(manifest: &[u8]) -> Result<Vec<ServiceDefinition>> {
    let services = extract_services(manifest)?;
    if !services.is_empty() {
        return Ok(services);
    }
    let docs = crate::manifest_yaml::parse_yaml_documents_from_slice(manifest)?;
    let mut ports = BTreeMap::new();
    for doc in docs {
        let doc = serde_json::to_value(doc)?;
        let spec = doc
            .pointer("/spec/template/spec")
            .or_else(|| doc.get("spec"));
        if let Some(containers) = spec
            .and_then(|spec| spec.get("containers"))
            .and_then(Value::as_array)
        {
            for container in containers {
                for port in container
                    .get("ports")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if port
                        .get("protocol")
                        .and_then(Value::as_str)
                        .is_none_or(|protocol| protocol == "TCP")
                    {
                        let port = number(&port["containerPort"])?;
                        ports.insert(
                            port,
                            ServicePort {
                                port,
                                target_port: port,
                            },
                        );
                    }
                }
            }
        }
    }
    if ports.is_empty() {
        ports.insert(
            18080,
            ServicePort {
                port: 18080,
                target_port: 18080,
            },
        );
    }
    ensure!(
        ports.len() <= MAX_SERVICE_PORTS,
        "too many workload TCP ports"
    );
    Ok(vec![ServiceDefinition {
        name: "workload".into(),
        ports: ports.into_values().collect(),
    }])
}
fn number(value: &Value) -> Result<u16> {
    let number = value.as_u64().context("Service port must be an integer")?;
    ensure!((1..=65535).contains(&number), "Service port out of range");
    Ok(number as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MANIFEST: &[u8] = b"kind: Pod\nmetadata: {name: app, labels: {app: api}}\nspec:\n  containers:\n    - name: app\n      image: nginx\n      ports: [{name: http, containerPort: 8080}]\n---\nkind: Service\nmetadata: {name: api}\nspec:\n  selector: {app: api}\n  ports: [{port: 80, targetPort: http}]\n";
    #[test]
    fn compiles_service_without_ingress_and_validates_selector_target_and_protocol() {
        let services = extract_services(MANIFEST).unwrap();
        assert_eq!(
            services[0].ports[0],
            ServicePort {
                port: 80,
                target_port: 8080
            }
        );
        let source = std::str::from_utf8(MANIFEST).unwrap();
        for invalid in [
            source.replace("selector: {app: api}", "selector: {app: other}"),
            source.replace("targetPort: http", "targetPort: absent"),
            source.replace("port: 80", "port: 80, protocol: UDP"),
        ] {
            assert!(extract_services(invalid.as_bytes()).is_err());
        }
    }
}
