use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::cell::Cell;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static FZY_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static FZY_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct FzyRuntimeGuard<'a> {
    outermost: bool,
    previous_depth: usize,
    _lock: Option<MutexGuard<'a, ()>>,
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

impl Drop for FzyRuntimeGuard<'_> {
    fn drop(&mut self) {
        if self.outermost {
            FZY_DEPTH.with(|slot| slot.set(0));
            unsafe {
                let _ = fz_host_shutdown();
                let _ = fz_host_cleanup();
            }
        } else {
            FZY_DEPTH.with(|slot| slot.set(self.previous_depth));
        }
    }
}

fn enter_fzy_runtime() -> FzyRuntimeGuard<'static> {
    crate::host_abi::link_host_abi();
    let depth = FZY_DEPTH.with(Cell::get);
    if depth > 0 {
        FZY_DEPTH.with(|slot| slot.set(depth + 1));
        return FzyRuntimeGuard {
            outermost: false,
            previous_depth: depth,
            _lock: None,
        };
    }

    let guard = FZY_LOCK.lock().expect("fzy lock poisoned");
    unsafe {
        let _ = fz_host_init();
    }
    FZY_DEPTH.with(|slot| slot.set(1));
    FzyRuntimeGuard {
        outermost: true,
        previous_depth: 0,
        _lock: Some(guard),
    }
}

fn with_fzy_runtime<T>(f: impl FnOnce() -> T) -> T {
    let _guard = enter_fzy_runtime();
    f()
}

pub fn planner_schema_version() -> i32 {
    with_fzy_runtime(|| unsafe { megaserver_fzy_schema_version() })
}

pub fn run_planner_with_io(input_payload: &str, output_path: &Path) -> std::io::Result<i32> {
    with_fzy_runtime(|| {
        let planner_input = scratch_path("megaserver.fzy.plan.input.json");
        let planner_output = scratch_path("megaserver.fzy.plan.output.json");
        fs::create_dir_all(planner_input.parent().expect("planner input parent"))?;
        unsafe {
            env::set_var("MEGASERVER_FZY_PLAN_INPUT", &planner_input);
            env::set_var("MEGASERVER_FZY_PLAN_OUTPUT", &planner_output);
        }
        fs::write(&planner_input, input_payload)?;
        let _ = fs::remove_file(&planner_output);
        let code = unsafe { megaserver_fzy_plan_manifest() };
        if code == 0 {
            fs::copy(&planner_output, output_path)?;
        }
        Ok(code)
    })
}

pub fn dispatch_control(payload: &Value) -> Result<Value> {
    let input = serde_json::to_string(payload)?;
    with_fzy_runtime(|| {
        let control_input = scratch_path("megaserver.fzy.control.input.json");
        let control_output = scratch_path("megaserver.fzy.control.output.json");
        let host_input = scratch_path("megaserver.fzy.host.input.json");
        let host_output = scratch_path("megaserver.fzy.host.output.json");
        fs::create_dir_all(control_input.parent().expect("control input parent"))?;
        unsafe {
            env::set_var("MEGASERVER_FZY_CONTROL_INPUT", &control_input);
            env::set_var("MEGASERVER_FZY_CONTROL_OUTPUT", &control_output);
            env::set_var("MEGASERVER_FZY_HOST_INPUT", &host_input);
            env::set_var("MEGASERVER_FZY_HOST_OUTPUT", &host_output);
        }
        fs::write(&control_input, input).context("write Fzy control input")?;
        let _ = fs::remove_file(&control_output);
        let _ = fs::remove_file(&host_output);
        let code = unsafe { megaserver_fzy_dispatch_control() };
        let output = fs::read_to_string(&control_output).unwrap_or_default();
        if code != 0 && output.is_empty() {
            let message = last_error_message();
            return Err(anyhow!(
                "Fzy control plane dispatch failed with code {code}: {message}"
            ));
        }
        serde_json::from_str::<Value>(&output).context("parse Fzy control output")
    })
}

fn scratch_path(file_name: &str) -> PathBuf {
    env::temp_dir()
        .join(format!("megaserver-fzy-{}", std::process::id()))
        .join(file_name)
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
