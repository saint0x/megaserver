use crate::ffi;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};
use std::path::PathBuf;

pub fn dispatch(home: &PathBuf, action: &str, fields: &[(&str, Value)]) -> Result<Value> {
    let mut payload = Map::new();
    payload.insert(
        "home".to_string(),
        Value::String(home.display().to_string()),
    );
    payload.insert("action".to_string(), Value::String(action.to_string()));
    for (key, value) in fields {
        payload.insert((*key).to_string(), value.clone());
    }
    ffi::dispatch_control(&Value::Object(payload))
}

pub fn dispatch_http(
    home: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let mut payload = Map::new();
    payload.insert(
        "home".to_string(),
        Value::String(home.display().to_string()),
    );
    payload.insert("method".to_string(), Value::String(method.to_string()));
    payload.insert("path".to_string(), Value::String(path.to_string()));
    if let Some(value) = body {
        payload.insert(
            "body".to_string(),
            Value::String(serde_json::to_string(value).context("encode HTTP control body")?),
        );
    }
    ffi::dispatch_control(&Value::Object(payload))
}

pub fn require_status_ok(value: Value) -> Result<Value> {
    if value.get("status").and_then(Value::as_str) == Some("error") {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown control plane failure");
        bail!("{message}");
    }
    if matches!(value, Value::Null) {
        return Err(anyhow!("control plane returned null"));
    }
    Ok(value)
}

pub fn dispatch_ok(home: &PathBuf, action: &str, fields: &[(&str, Value)]) -> Result<Value> {
    require_status_ok(dispatch(home, action, fields)?)
        .with_context(|| format!("dispatch `{action}`"))
}

pub fn dispatch_http_ok(
    home: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    require_status_ok(dispatch_http(home, method, path, body)?)
        .with_context(|| format!("dispatch HTTP `{method} {path}`"))
}
