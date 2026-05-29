use crate::ffi;
use crate::manifest::ServiceManifest;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub fn plan_manifest(home: &Path, manifest: &ServiceManifest) -> Result<Value> {
    let planner_dir = home.join("planner");
    fs::create_dir_all(&planner_dir)
        .with_context(|| format!("create {}", planner_dir.display()))?;
    let input = planner_dir.join("manifest.input.json");
    let output = planner_dir.join("manifest.plan.json");
    let input_payload = serde_json::to_string_pretty(manifest)?;
    fs::write(&input, input_payload.as_bytes())
        .with_context(|| format!("write {}", input.display()))?;

    let schema_version = ffi::planner_schema_version();
    let code = ffi::run_planner_with_io(&input_payload, &output)
        .with_context(|| format!("invoke Fzy planner for {}", output.display()))?;
    if code != 0 {
        let last_error = ffi::last_error_message();
        if last_error.is_empty() {
            bail!("Fzy planner returned non-zero status: {code}");
        }
        bail!("Fzy planner returned non-zero status: {code}: {last_error}");
    }

    let raw = fs::read_to_string(&output)
        .with_context(|| format!("read planner output {}", output.display()))?;
    let planned: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse planner output {}", output.display()))?;
    Ok(serde_json::json!({
        "planner": "fzy",
        "schema_version": schema_version,
        "planner_status": "ok",
        "plan": planned
    }))
}
