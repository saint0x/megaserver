use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRecord {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub app_path: String,
    pub manifest_json: String,
    pub plan_json: String,
    pub pid: Option<i32>,
    pub port: Option<u16>,
    pub health_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: i64,
    pub name: String,
    pub service_name: String,
    pub source_kind: String,
    pub source_ref: String,
    pub app_path: String,
    pub manifest_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub id: i64,
    pub project_name: String,
    pub service_name: String,
    pub app_path: String,
    pub manifest_json: String,
    pub plan_json: String,
    pub source_kind: String,
    pub source_ref: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRecord {
    pub id: i64,
    pub service_name: Option<String>,
    pub name: String,
    pub host_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRecord {
    pub id: i64,
    pub service_name: String,
    pub domain: String,
    pub port: Option<u16>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRecord {
    pub id: i64,
    pub service_name: String,
    pub key: String,
    pub value: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub id: i64,
    pub service_name: String,
    pub snapshot_name: String,
    pub snapshot_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRecord {
    pub id: i64,
    pub service_name: String,
    pub sandbox_id: String,
    pub hostname: String,
    pub ip_address: Option<String>,
    pub runtime_kind: String,
    pub isolation_mode: String,
    pub status: String,
    pub pid: Option<i32>,
    pub cgroup_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: i64,
    pub service_name: Option<String>,
    pub kind: String,
    pub details_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceBundle {
    pub service: ServiceRecord,
    pub logs_dir: PathBuf,
    pub volumes_dir: PathBuf,
}
