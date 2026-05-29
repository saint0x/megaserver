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

pub fn dispatch_http_raw(
    home: &PathBuf,
    method: &str,
    path: &str,
    body: Option<&str>,
    content_type: Option<&str>,
    accept: Option<&str>,
    authorization: Option<&str>,
) -> Result<Value> {
    let mut payload = Map::new();
    payload.insert(
        "home".to_string(),
        Value::String(home.display().to_string()),
    );
    payload.insert("method".to_string(), Value::String(method.to_string()));
    payload.insert("path".to_string(), Value::String(path.to_string()));
    if let Some(value) = body {
        payload.insert("body".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = content_type {
        payload.insert("content_type".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = accept {
        payload.insert("accept".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = authorization {
        payload.insert(
            "authorization".to_string(),
            Value::String(value.to_string()),
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
    let value = dispatch_http(home, method, path, body)?;
    if let Some(status) = value.get("http_status").and_then(Value::as_u64) {
        if status >= 400 {
            let parsed_body = value.get("body").and_then(|body| {
                if let Some(text) = body.as_str() {
                    serde_json::from_str::<Value>(text).ok()
                } else {
                    Some(body.clone())
                }
            });
            let message = parsed_body
                .as_ref()
                .and_then(|body| body.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("unknown control plane HTTP failure");
            bail!("{message}");
        }
        if let Some(body) = value.get("body") {
            if let Some(text) = body.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    return Ok(parsed);
                }
            }
            return Ok(body.clone());
        }
    }
    require_status_ok(value).with_context(|| format!("dispatch HTTP `{method} {path}`"))
}
