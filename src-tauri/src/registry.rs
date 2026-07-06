use serde::Deserialize;
use thiserror::Error;

const REGISTRY_BASE: &str = "https://registry.npmjs.org";
const PACKAGE: &str = "omniroute";

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("network error: {0}")]
    Network(String),
    #[error("version {0} not found in registry")]
    VersionNotFound(String),
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: String,
}

#[derive(Debug, Deserialize)]
struct PackageDoc {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
}

#[derive(Debug, Deserialize)]
struct Engines {
    node: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VersionDoc {
    engines: Option<Engines>,
}

pub fn latest_version() -> Result<String, RegistryError> {
    let url = format!("{REGISTRY_BASE}/{PACKAGE}");
    let doc: PackageDoc = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(8))
        .call()
        .map_err(|e| RegistryError::Network(e.to_string()))?
        .into_json()
        .map_err(|e| RegistryError::Network(e.to_string()))?;
    Ok(doc.dist_tags.latest)
}

pub fn engines_node(version: &str) -> Result<Option<String>, RegistryError> {
    let url = format!("{REGISTRY_BASE}/{PACKAGE}/{version}");
    let response = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(8))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => RegistryError::VersionNotFound(version.to_string()),
            other => RegistryError::Network(other.to_string()),
        })?;
    let doc: VersionDoc = response
        .into_json()
        .map_err(|e| RegistryError::Network(e.to_string()))?;
    Ok(doc.engines.and_then(|e| e.node))
}
