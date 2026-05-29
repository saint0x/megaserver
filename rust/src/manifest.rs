use anyhow::{Context, Result, bail};
use ipnet::IpNet;
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
    #[serde(default)]
    pub policy: NetworkPolicySection,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicySection {
    #[serde(default)]
    pub ingress: NetworkAccessPolicy,
    #[serde(default)]
    pub egress: NetworkAccessPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkAccessPolicy {
    #[serde(default)]
    pub default: NetworkPolicyDefault,
    #[serde(default)]
    pub allow_services: Vec<String>,
    #[serde(default)]
    pub allow_cidrs: Vec<String>,
    #[serde(default)]
    pub deny_services: Vec<String>,
    #[serde(default)]
    pub deny_cidrs: Vec<String>,
}

impl Default for NetworkAccessPolicy {
    fn default() -> Self {
        Self {
            default: NetworkPolicyDefault::Allow,
            allow_services: Vec::new(),
            allow_cidrs: Vec::new(),
            deny_services: Vec::new(),
            deny_cidrs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicyDefault {
    #[default]
    Allow,
    Deny,
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
    validate_network_policy(&manifest)?;

    Ok((manifest_path, manifest))
}

fn validate_network_policy(manifest: &ServiceManifest) -> Result<()> {
    for (direction, policy) in [
        ("network.policy.ingress", &manifest.network.policy.ingress),
        ("network.policy.egress", &manifest.network.policy.egress),
    ] {
        validate_service_list(direction, "allow_services", &policy.allow_services)?;
        validate_service_list(direction, "deny_services", &policy.deny_services)?;
        validate_cidr_list(direction, "allow_cidrs", &policy.allow_cidrs)?;
        validate_cidr_list(direction, "deny_cidrs", &policy.deny_cidrs)?;
    }
    Ok(())
}

fn validate_service_list(direction: &str, field: &str, values: &[String]) -> Result<()> {
    for value in values {
        if value.trim().is_empty() {
            bail!("manifest `{direction}.{field}` cannot contain an empty service name");
        }
    }
    Ok(())
}

fn validate_cidr_list(direction: &str, field: &str, values: &[String]) -> Result<()> {
    for value in values {
        value
            .parse::<IpNet>()
            .with_context(|| format!("invalid CIDR `{value}` in `{direction}.{field}`"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_policy_defaults_to_allow() {
        let manifest: ServiceManifest = serde_yaml::from_str(
            r#"
name: demo
runtime:
  command: ["python3", "server.py"]
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.network.policy.ingress.default,
            NetworkPolicyDefault::Allow
        );
        assert_eq!(
            manifest.network.policy.egress.default,
            NetworkPolicyDefault::Allow
        );
        assert!(manifest.network.policy.ingress.allow_services.is_empty());
    }

    #[test]
    fn parses_network_policy_sections() {
        let manifest: ServiceManifest = serde_yaml::from_str(
            r#"
name: demo
runtime:
  command: ["python3", "server.py"]
network:
  port: 8080
  policy:
    ingress:
      default: deny
      allow_services: ["frontend"]
      allow_cidrs: ["10.0.0.0/8"]
    egress:
      deny_services: ["db"]
      deny_cidrs: ["169.254.169.254/32"]
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.network.policy.ingress.default,
            NetworkPolicyDefault::Deny
        );
        assert_eq!(
            manifest.network.policy.ingress.allow_services,
            vec!["frontend"]
        );
        assert_eq!(
            manifest.network.policy.egress.deny_cidrs,
            vec!["169.254.169.254/32"]
        );
    }
}
