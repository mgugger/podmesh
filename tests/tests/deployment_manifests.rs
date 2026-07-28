use anyhow::{Context, Result, ensure};
use serde::Deserialize;

const ROOTFUL: &str = include_str!("../../deploy/podmesh_rootful.yml");
const ROOTLESS: &str = include_str!("../../deploy/podmesh_rootless.yml");

#[test]
fn deployment_manifests_are_valid_multi_document_yaml() -> Result<()> {
    for (name, manifest) in [("rootful", ROOTFUL), ("rootless", ROOTLESS)] {
        let documents = serde_yaml::Deserializer::from_str(manifest)
            .map(|document| {
                serde_yaml::Value::deserialize(document)
                    .with_context(|| format!("parse {name} deployment document"))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !documents.is_empty(),
            "{name} deployment must contain at least one document"
        );
        ensure!(
            documents.iter().all(|document| document
                .get("kind")
                .and_then(serde_yaml::Value::as_str)
                .is_some()),
            "{name} deployment contains a document without kind"
        );
    }
    Ok(())
}
