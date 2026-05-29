use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceManifest {
    pub name: String,
    #[serde(default)]
    pub runtime: RuntimeSection,
    #[serde(default)]
    pub network: NetworkSection,
    #[serde(default)]
    pub resources: ResourceSection,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub routes: Vec<String>,
    #[serde(default)]
    pub health: HealthSection,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeSection {
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkSection {
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceSection {
    pub memory: Option<String>,
    pub cpu: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthSection {
    pub path: Option<String>,
}

pub fn load_manifest(app_path: &Path) -> Result<(PathBuf, ServiceManifest)> {
    let candidates = [
        app_path.join("megaserver.yml"),
        app_path.join("megaserver.yaml"),
        app_path.join("Megaserver.yml"),
        app_path.join("Megaserver.yaml"),
    ];

    let manifest_path = candidates
        .into_iter()
        .find(|path| path.exists())
        .context("missing megaserver manifest; expected megaserver.yml or megaserver.yaml")?;

    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
    let manifest: ServiceManifest = serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse manifest {}", manifest_path.display()))?;

    if manifest.name.trim().is_empty() {
        bail!("manifest `name` cannot be empty");
    }
    if manifest.runtime.command.is_empty() {
        bail!("manifest `runtime.command` must contain at least one program");
    }

    Ok((manifest_path, manifest))
}
