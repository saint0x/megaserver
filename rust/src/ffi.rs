use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static FZY_LOCK: Mutex<()> = Mutex::new(());
const PLANNER_INPUT: &str = "/tmp/megaserver.fzy.plan.input.json";
const PLANNER_OUTPUT: &str = "/tmp/megaserver.fzy.plan.output.json";
const CONTROL_INPUT: &str = "/tmp/megaserver.fzy.control.input.json";
const CONTROL_OUTPUT: &str = "/tmp/megaserver.fzy.control.output.json";

thread_local! {
    static FZY_DEPTH: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" {
    fn fz_host_init() -> i32;
    fn fz_host_shutdown() -> i32;
    fn fz_host_cleanup() -> i32;
    fn fz_host_last_error_message() -> *const std::ffi::c_char;
    fn megaserver_fzy_schema_version() -> i32;
    fn megaserver_fzy_plan_manifest() -> i32;
    fn megaserver_fzy_dispatch_control() -> i32;
}

fn with_fzy_runtime<T>(f: impl FnOnce() -> T) -> T {
    crate::host_abi::link_host_abi();
    let depth = FZY_DEPTH.with(Cell::get);
    if depth > 0 {
        FZY_DEPTH.with(|slot| slot.set(depth + 1));
        let result = f();
        FZY_DEPTH.with(|slot| slot.set(depth));
        return result;
    }

    let _guard = FZY_LOCK.lock().expect("fzy lock poisoned");
    unsafe {
        let _ = fz_host_init();
    }
    FZY_DEPTH.with(|slot| slot.set(1));
    let result = f();
    FZY_DEPTH.with(|slot| slot.set(0));
    unsafe {
        let _ = fz_host_shutdown();
        let _ = fz_host_cleanup();
    }
    result
}

pub fn planner_schema_version() -> i32 {
    with_fzy_runtime(|| unsafe { megaserver_fzy_schema_version() })
}

pub fn run_planner_with_io(input_payload: &str, output_path: &Path) -> std::io::Result<i32> {
    with_fzy_runtime(|| {
        fs::write(PLANNER_INPUT, input_payload)?;
        let _ = fs::remove_file(PLANNER_OUTPUT);
        let code = unsafe { megaserver_fzy_plan_manifest() };
        if code == 0 {
            fs::copy(PLANNER_OUTPUT, output_path)?;
        }
        Ok(code)
    })
}

pub fn dispatch_control(payload: &Value) -> Result<Value> {
    let input = serde_json::to_string(payload)?;
    with_fzy_runtime(|| {
        fs::write(CONTROL_INPUT, input).context("write Fzy control input")?;
        let _ = fs::remove_file(CONTROL_OUTPUT);
        let code = unsafe { megaserver_fzy_dispatch_control() };
        let output = fs::read_to_string(CONTROL_OUTPUT).unwrap_or_default();
        if code != 0 && output.is_empty() {
            let message = last_error_message();
            return Err(anyhow!(
                "Fzy control plane dispatch failed with code {code}: {message}"
            ));
        }
        serde_json::from_str::<Value>(&output).context("parse Fzy control output")
    })
}

pub fn last_error_message() -> String {
    unsafe {
        let ptr = fz_host_last_error_message();
        if ptr.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}
