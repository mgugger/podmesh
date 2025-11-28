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
