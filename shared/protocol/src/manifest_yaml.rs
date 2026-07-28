use serde::Deserialize;
use serde_json::Value as JsonValue;

pub fn parse_yaml_documents_from_slice(
    bytes: &[u8],
) -> Result<Vec<serde_yaml::Value>, serde_yaml::Error> {
    parse_yaml_documents(serde_yaml::Deserializer::from_slice(bytes))
}

pub fn parse_yaml_documents_from_str(
    contents: &str,
) -> Result<Vec<serde_yaml::Value>, serde_yaml::Error> {
    parse_yaml_documents(serde_yaml::Deserializer::from_str(contents))
}

pub fn parse_manifest_to_json(contents: &str) -> anyhow::Result<JsonValue> {
    let mut docs = Vec::new();

    for document in serde_yaml::Deserializer::from_str(contents) {
        let yaml_value = serde_yaml::Value::deserialize(document)?;
        if yaml_value.is_null() {
            continue;
        }
        let json_value = serde_json::to_value(yaml_value)?;
        docs.push(json_value);
    }

    if docs.is_empty() {
        anyhow::bail!("manifest file did not contain any YAML documents");
    }

    if docs.len() == 1 {
        Ok(docs.into_iter().next().unwrap())
    } else {
        Ok(JsonValue::Array(docs))
    }
}

fn parse_yaml_documents<'de, I>(iter: I) -> Result<Vec<serde_yaml::Value>, serde_yaml::Error>
where
    I: IntoIterator<Item = serde_yaml::Deserializer<'de>>,
{
    let mut docs = Vec::new();
    for document in iter {
        let value = serde_yaml::Value::deserialize(document)?;
        if value.is_null() {
            continue;
        }
        docs.push(value);
    }
    Ok(docs)
}

/// Serialize YAML documents back into a single string separated with `---` delimiters.
pub fn serialize_yaml_documents(docs: &[serde_yaml::Value]) -> Result<String, serde_yaml::Error> {
    let mut out = String::new();
    for (idx, doc) in docs.iter().enumerate() {
        if idx > 0 {
            out.push_str("---\n");
        }
        let mut rendered = serde_yaml::to_string(doc)?;
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        out.push_str(&rendered);
    }
    Ok(out)
}

/// Reads the owner's desired replica count and rewrites every document to run a
/// single pod.
///
/// Podmesh spreads replicas across agents rather than within one agent: the
/// client deploys the same manifest to N distinct agents and each of them runs
/// exactly one pod. Pinning `spec.replicas` to 1 keeps the agent's local
/// container runtime from multiplying that count again.
///
/// The count is taken from `spec.replicas` and from the
/// `podmesh.io/replicas` annotation, whichever asks for more, then clamped to
/// `[1, MAX_WORKLOAD_REPLICAS]`.
pub fn normalize_replicas(docs: &mut [serde_yaml::Value]) -> u32 {
    const REPLICAS: &str = "replicas";
    let mut desired = 0u64;
    for doc in docs.iter_mut() {
        if let Some(annotated) = doc
            .get("metadata")
            .and_then(|metadata| metadata.get("annotations"))
            .and_then(|annotations| annotations.get(crate::PodmeshAnnotations::REPLICAS))
            .and_then(annotation_as_u64)
        {
            desired = desired.max(annotated);
        }
        let Some(spec) = doc
            .get_mut("spec")
            .and_then(serde_yaml::Value::as_mapping_mut)
        else {
            continue;
        };
        let key = serde_yaml::Value::String(REPLICAS.to_string());
        let Some(value) = spec.get(&key).and_then(serde_yaml::Value::as_u64) else {
            continue;
        };
        desired = desired.max(value);
        spec.insert(key, serde_yaml::Value::Number(1.into()));
    }
    desired.clamp(1, u64::from(crate::agent::MAX_WORKLOAD_REPLICAS)) as u32
}

/// Annotations are strings in Kubernetes but tolerate a plain YAML integer.
fn annotation_as_u64(value: &serde_yaml::Value) -> Option<u64> {
    match value {
        serde_yaml::Value::String(text) => text.trim().parse().ok(),
        other => other.as_u64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicas_are_lifted_out_of_the_manifest_and_pinned_to_one() {
        let mut docs = parse_yaml_documents_from_str(
            "kind: Deployment\nmetadata:\n  name: demo\nspec:\n  replicas: 3\n  template:\n    spec:\n      containers: []\n",
        )
        .unwrap();
        assert_eq!(normalize_replicas(&mut docs), 3);
        assert_eq!(
            docs[0].get("spec").and_then(|spec| spec.get("replicas")),
            Some(&serde_yaml::Value::Number(1.into()))
        );
    }

    #[test]
    fn manifests_without_replicas_default_to_a_single_pod() {
        let mut docs = parse_yaml_documents_from_str(
            "kind: Pod\nmetadata:\n  name: demo\nspec:\n  containers: []\n",
        )
        .unwrap();
        assert_eq!(normalize_replicas(&mut docs), 1);
        assert!(
            docs[0]
                .get("spec")
                .and_then(|spec| spec.get("replicas"))
                .is_none()
        );
    }

    #[test]
    fn replica_requests_are_clamped_to_the_protocol_maximum() {
        let mut docs = parse_yaml_documents_from_str(
            "kind: Deployment\nmetadata:\n  name: demo\nspec:\n  replicas: 100000\n",
        )
        .unwrap();
        assert_eq!(
            normalize_replicas(&mut docs),
            crate::agent::MAX_WORKLOAD_REPLICAS
        );
    }
}
