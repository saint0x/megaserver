use crate::app;
use crate::manifest::load_manifest;
use crate::runtime;
use crate::state::{self, StatePaths};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use flate2::read::GzDecoder;
use serde_json::json;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tar::Archive;

pub fn list_projects_value(paths: &StatePaths) -> Result<serde_json::Value> {
    let conn = state::open(paths)?;
    Ok(json!(state::list_projects(&conn)?))
}

pub fn list_deployments_value(
    paths: &StatePaths,
    project: Option<&str>,
    service: Option<&str>,
) -> Result<serde_json::Value> {
    let conn = state::open(paths)?;
    Ok(json!(state::list_deployment_releases(
        &conn, project, service
    )?))
}

pub fn import_project_value(
    paths: &StatePaths,
    source_path: &Path,
    project_name: Option<&str>,
) -> Result<serde_json::Value> {
    let source_path = source_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", source_path.display()))?;
    let source_ref = source_path.display().to_string();
    let project = stage_project_tree(paths, project_name, "path", &source_ref, &source_path)?;
    Ok(json!({"status": "ok", "project": project}))
}

pub fn upload_project_value(
    paths: &StatePaths,
    archive_base64: &str,
    format: &str,
    project_name: Option<&str>,
) -> Result<serde_json::Value> {
    let unpack_root = unpack_uploaded_project(paths, archive_base64, format)?;
    let source_root = locate_project_root(&unpack_root)?;
    let source_ref = format!("upload:{}:{}", format, state::now_string());
    let project = stage_project_tree(paths, project_name, "upload", &source_ref, &source_root)?;
    Ok(json!({"status": "ok", "project": project}))
}

pub fn deploy_project_value(
    paths: &StatePaths,
    project_name: &str,
    start: bool,
) -> Result<serde_json::Value> {
    let conn = state::open(paths)?;
    let project = state::project_by_name(&conn, project_name)?
        .ok_or_else(|| anyhow!("unknown project `{project_name}`"))?;
    drop(conn);

    let app_path = PathBuf::from(&project.app_path);
    let service = if start {
        app::deploy_and_start_only(paths, &app_path)?
    } else {
        app::deploy_only(paths, &app_path)?;
        let (_, manifest) = load_manifest(&app_path)?;
        manifest.name
    };
    let deployed = app::inspect_value(paths, &service)?;
    let service_record = deployed
        .get("service")
        .ok_or_else(|| anyhow!("missing deployed service payload"))?;
    let manifest_json = service_record
        .get("manifest_json")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("missing deployed manifest_json"))?;
    let plan_json = service_record
        .get("plan_json")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("missing deployed plan_json"))?;

    let conn = state::open(paths)?;
    state::insert_deployment_release(
        &conn,
        &project.name,
        &service,
        &app_path,
        manifest_json,
        plan_json,
        &project.source_kind,
        &project.source_ref,
    )?;
    Ok(json!({
        "status": "ok",
        "project": project.name,
        "service": deployed["service"].clone(),
        "sandbox": deployed["sandbox"].clone(),
    }))
}

fn stage_project_tree(
    paths: &StatePaths,
    project_name: Option<&str>,
    source_kind: &str,
    source_ref: &str,
    source_root: &Path,
) -> Result<crate::model::ProjectRecord> {
    let (_, manifest) = load_manifest(source_root)?;
    let project_name = sanitize_project_name(project_name.unwrap_or(&manifest.name))?;
    let release_root = paths
        .projects_dir
        .join(&project_name)
        .join("releases")
        .join(state::now_string());
    let app_root = release_root.join("app");
    fs::create_dir_all(&app_root)
        .with_context(|| format!("create staged project root {}", app_root.display()))?;
    runtime::copy_tree(source_root, &app_root)?;
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let conn = state::open(paths)?;
    state::upsert_project(
        &conn,
        &project_name,
        &manifest.name,
        source_kind,
        source_ref,
        &app_root,
        &manifest_json,
    )?;
    state::project_by_name(&conn, &project_name)?
        .ok_or_else(|| anyhow!("project `{project_name}` missing after upsert"))
}

fn unpack_uploaded_project(
    paths: &StatePaths,
    archive_base64: &str,
    format: &str,
) -> Result<PathBuf> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(archive_base64.trim())
        .context("decode project archive_base64")?;
    let root = paths
        .run_dir
        .join("project-upload")
        .join(state::now_string());
    fs::create_dir_all(&root)?;
    match format {
        "tar.gz" | "tgz" => {
            let decoder = GzDecoder::new(Cursor::new(bytes));
            let mut archive = Archive::new(decoder);
            archive
                .unpack(&root)
                .with_context(|| format!("unpack project archive into {}", root.display()))?;
        }
        "tar" => {
            let mut archive = Archive::new(Cursor::new(bytes));
            archive
                .unpack(&root)
                .with_context(|| format!("unpack project archive into {}", root.display()))?;
        }
        other => bail!("unsupported project archive format `{other}`"),
    }
    Ok(root)
}

fn locate_project_root(root: &Path) -> Result<PathBuf> {
    let mut matches = Vec::new();
    find_manifest_roots(root, &mut matches)?;
    match matches.len() {
        0 => bail!("uploaded project archive does not contain a megaserver manifest"),
        1 => Ok(matches.remove(0)),
        _ => bail!("uploaded project archive contains multiple megaserver manifests"),
    }
}

fn find_manifest_roots(root: &Path, matches: &mut Vec<PathBuf>) -> Result<()> {
    if has_manifest(root) {
        matches.push(root.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            find_manifest_roots(&entry.path(), matches)?;
        }
    }
    Ok(())
}

fn has_manifest(root: &Path) -> bool {
    [
        root.join("megaserver.yml"),
        root.join("megaserver.yaml"),
        root.join("Megaserver.yml"),
        root.join("Megaserver.yaml"),
    ]
    .into_iter()
    .any(|path| path.exists())
}

fn sanitize_project_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("project name cannot be empty");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("project name `{trimmed}` contains unsupported characters");
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::TempDir;

    #[test]
    fn upload_project_archive_imports_first_class_project() {
        let temp = TempDir::new().unwrap();
        let paths = StatePaths::resolve(Some(temp.path().join("home"))).unwrap();
        state::init(&paths).unwrap();

        let src = temp.path().join("src-app");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("server.py"), "print('ok')\n").unwrap();
        fs::write(
            src.join("megaserver.yaml"),
            "name: uploaded-app\nruntime:\n  command:\n    - python3\n    - server.py\nnetwork:\n  port: 18080\nvolumes: []\nroutes: []\nhealth:\n  path: /health\n",
        )
        .unwrap();

        let mut tar_bytes = Vec::new();
        {
            let encoder = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut builder = tar::Builder::new(encoder);
            builder.append_dir_all("package", &src).unwrap();
            let encoder = builder.into_inner().unwrap();
            encoder.finish().unwrap();
        }
        let payload = STANDARD.encode(tar_bytes);
        let value = upload_project_value(&paths, &payload, "tar.gz", None).unwrap();
        assert_eq!(value["project"]["name"].as_str(), Some("uploaded-app"));
        let staged = value["project"]["app_path"].as_str().unwrap();
        assert!(staged.contains("/projects/uploaded-app/releases/"));
    }
}
