use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use protocol::{
    libp2p_constants::MESH_DOMAIN_SUFFIX,
    machine::{SidecarRouteKind, SidecarRouteSpec},
};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteExtraction {
    pub routes: Vec<SidecarRouteSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceInfo {
    name: String,
    ports: Vec<ServicePort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServicePort {
    name: Option<String>,
    port: u16,
    target: Option<TargetPort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetPort {
    Number(u16),
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IngressPath {
    host: String,
    path: String,
    service_name: String,
    selector: ServicePortSelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServicePortSelector {
    Name(String),
    Number(u16),
}

pub fn extract_sidecar_routes(manifest: &[u8], manifest_id: &str) -> Result<RouteExtraction> {
    let mut services: HashMap<String, ServiceInfo> = HashMap::new();
    let mut ingress_paths: Vec<IngressPath> = Vec::new();
    let mut container_ports: HashMap<String, u16> = HashMap::new();

    for doc in serde_yaml::Deserializer::from_slice(manifest) {
        let value: Value = Value::deserialize(doc)?;
        match value.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "Service" => {
                if let Some(info) = parse_service(&value)? {
                    services.insert(info.name.clone(), info);
                }
            }
            "Ingress" => {
                ingress_paths.extend(parse_ingress(&value)?);
            }
            kind if has_pod_template(kind) => {
                collect_container_ports(&value, &mut container_ports);
            }
            _ => {}
        }
    }

    let mut routes = Vec::new();
    for svc in services.values() {
        let host = format_service_host(&svc.name, manifest_id);
        for port in &svc.ports {
            let target_port = resolve_service_target(port, &container_ports)?;
            routes.push(SidecarRouteSpec {
                host: host.clone(),
                path_prefix: "/".to_string(),
                target_port,
                service_name: svc.name.clone(),
                service_port: port.name.clone().unwrap_or_else(|| port.port.to_string()),
                source: SidecarRouteKind::Service,
            });
        }
    }

    for ingress in ingress_paths {
        let service = services.get(&ingress.service_name).ok_or_else(|| {
            anyhow!(
                "ingress host '{}' references unknown service '{}'",
                ingress.host,
                ingress.service_name
            )
        })?;
        let svc_port = find_service_port(service, &ingress.selector).ok_or_else(|| {
            anyhow!(
                "ingress host '{}' references missing port {:?} on service '{}'",
                ingress.host,
                ingress.selector,
                ingress.service_name
            )
        })?;
        let target_port = resolve_service_target(svc_port, &container_ports)?;
        routes.push(SidecarRouteSpec {
            host: ingress.host.clone(),
            path_prefix: ingress.path.clone(),
            target_port,
            service_name: ingress.service_name.clone(),
            service_port: svc_port
                .name
                .clone()
                .unwrap_or_else(|| svc_port.port.to_string()),
            source: SidecarRouteKind::Ingress,
        });
    }

    if routes.is_empty() {
        log::warn!(
            "manifest does not define services or ingress routes; starting sidecar without routes manifest={}",
            manifest_id
        );
    }

    Ok(RouteExtraction { routes })
}

fn parse_service(value: &Value) -> Result<Option<ServiceInfo>> {
    let metadata = match value.get("metadata").and_then(|m| m.as_mapping()) {
        Some(m) => m,
        None => return Ok(None),
    };
    let name = metadata
        .get(Value::from("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("service missing metadata.name"))?;

    let spec = value
        .get("spec")
        .and_then(|s| s.as_mapping())
        .ok_or_else(|| anyhow!("service {} missing spec", name))?;
    let ports = spec
        .get(Value::from("ports"))
        .and_then(|p| p.as_sequence())
        .ok_or_else(|| anyhow!("service {} missing spec.ports", name))?;

    let mut parsed_ports = Vec::new();
    for port_value in ports {
        let Some(port_map) = port_value.as_mapping() else {
            continue;
        };
        let Some(port_number) = port_map.get(Value::from("port")).and_then(parse_u16) else {
            bail!("service {} port entry missing 'port'", name);
        };
        let target = match port_map.get(Value::from("targetPort")) {
            Some(v) if v.is_u64() => Some(TargetPort::Number(parse_u16(v).unwrap_or(port_number))),
            Some(v) if v.is_string() => v.as_str().map(|s| TargetPort::Named(s.to_string())),
            _ => None,
        };
        let name_value = port_map
            .get(Value::from("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        parsed_ports.push(ServicePort {
            name: name_value,
            port: port_number,
            target,
        });
    }

    if parsed_ports.is_empty() {
        bail!("service {} does not define any ports", name);
    }

    Ok(Some(ServiceInfo {
        name,
        ports: parsed_ports,
    }))
}

fn parse_ingress(value: &Value) -> Result<Vec<IngressPath>> {
    let spec = match value.get("spec").and_then(|s| s.as_mapping()) {
        Some(spec) => spec,
        None => return Ok(Vec::new()),
    };
    let rules = match spec.get(Value::from("rules")) {
        Some(Value::Sequence(entries)) => entries,
        _ => return Ok(Vec::new()),
    };

    let mut paths = Vec::new();
    for rule in rules {
        let Some(host) = rule
            .get(Value::from("host"))
            .and_then(|h| h.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let normalized_host = host.to_lowercase();
        let http = match rule.get(Value::from("http")) {
            Some(v) => v,
            None => continue,
        };
        let http_paths = match http.get(Value::from("paths")) {
            Some(Value::Sequence(seq)) => seq,
            _ => continue,
        };
        for entry in http_paths {
            if let Some(path) = parse_ingress_path(entry, &normalized_host)? {
                paths.push(path);
            }
        }
    }

    Ok(paths)
}

fn parse_ingress_path(value: &Value, host: &str) -> Result<Option<IngressPath>> {
    let Some(path_map) = value.as_mapping() else {
        return Ok(None);
    };
    let path = path_map
        .get(Value::from("path"))
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "/".to_string());

    let backend = match path_map.get(Value::from("backend")) {
        Some(b) => b,
        None => return Ok(None),
    };
    let service = backend
        .get(Value::from("service"))
        .and_then(|s| s.as_mapping())
        .ok_or_else(|| anyhow!("ingress {} missing backend.service", host))?;
    let service_name = service
        .get(Value::from("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("ingress {} missing backend.service.name", host))?;

    let port_selector = match service.get(Value::from("port")) {
        Some(port_obj) if port_obj.is_mapping() => {
            if let Some(number) = port_obj.get(Value::from("number")).and_then(parse_u16) {
                ServicePortSelector::Number(number)
            } else if let Some(name) = port_obj
                .get(Value::from("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                ServicePortSelector::Name(name)
            } else {
                bail!("ingress {} missing backend.service.port selector", host)
            }
        }
        _ => bail!("ingress {} missing backend.service.port", host),
    };

    Ok(Some(IngressPath {
        host: host.to_string(),
        path: normalize_path(&path),
        service_name,
        selector: port_selector,
    }))
}

fn collect_container_ports(value: &Value, registry: &mut HashMap<String, u16>) {
    let spec = match extract_pod_spec(value) {
        Some(spec) => spec,
        None => return,
    };
    let containers = match spec.get(Value::from("containers")) {
        Some(Value::Sequence(seq)) => seq,
        _ => return,
    };
    for container in containers {
        let Some(mapping) = container.as_mapping() else {
            continue;
        };
        let ports = match mapping.get(Value::from("ports")) {
            Some(Value::Sequence(seq)) => seq,
            _ => continue,
        };
        for port in ports {
            if let Some(name) = port
                .get(Value::from("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
                && let Some(value) = port.get(Value::from("containerPort"))
                && let Some(number) = parse_u16(value)
            {
                registry.insert(name, number);
            }
        }
    }
}

fn extract_pod_spec(value: &Value) -> Option<&Mapping> {
    let mapping = value.as_mapping()?;
    match mapping.get(Value::from("kind")).and_then(|k| k.as_str())? {
        "Pod" => mapping.get(Value::from("spec"))?.as_mapping(),
        "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet" => mapping
            .get(Value::from("spec"))?
            .get(Value::from("template"))?
            .get(Value::from("spec"))?
            .as_mapping(),
        _ => None,
    }
}

fn has_pod_template(kind: &str) -> bool {
    matches!(
        kind,
        "Pod" | "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet"
    )
}

fn resolve_service_target(port: &ServicePort, names: &HashMap<String, u16>) -> Result<u16> {
    match &port.target {
        Some(TargetPort::Number(val)) => Ok(*val),
        Some(TargetPort::Named(name)) => names
            .get(name)
            .copied()
            .with_context(|| format!("service port references unknown container port '{name}'")),
        None => Ok(port.port),
    }
}

fn find_service_port<'a>(
    svc: &'a ServiceInfo,
    selector: &ServicePortSelector,
) -> Option<&'a ServicePort> {
    match selector {
        ServicePortSelector::Number(port) => svc.ports.iter().find(|p| &p.port == port),
        ServicePortSelector::Name(name) => svc
            .ports
            .iter()
            .find(|p| p.name.as_deref() == Some(name.as_str())),
    }
}

fn format_service_host(service: &str, manifest_id: &str) -> String {
    format!("{}.{}.{}", service, manifest_id, MESH_DOMAIN_SUFFIX).to_lowercase()
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn parse_u16(value: &Value) -> Option<u16> {
    if let Some(num) = value.as_u64() {
        return (num <= u16::MAX as u64).then_some(num as u16);
    }
    value.as_str().and_then(|s| s.parse::<u16>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_routes_from_sample_manifest() {
        let manifest = include_bytes!("../../podmesh-proxy/tests/sample_manifests/nginx.yml");
        let extraction =
            extract_sidecar_routes(manifest, "my-nginx").expect("route extraction succeeds");
        assert!(
            extraction
                .routes
                .iter()
                .any(|route| route.source == SidecarRouteKind::Service)
        );
        assert!(
            extraction
                .routes
                .iter()
                .any(|route| route.source == SidecarRouteKind::Ingress)
        );
        let expected_host = format!("demo-nginx.{}", MESH_DOMAIN_SUFFIX);
        assert!(
            extraction
                .routes
                .iter()
                .any(|route| route.host == expected_host)
        );
    }
}
