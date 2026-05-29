use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let repo_root = manifest_dir.parent().expect("repo root");

    let mut source_inputs = collect_fzy_inputs(&repo_root.join("src"));
    source_inputs.push(repo_root.join("fozzy.toml"));
    for input in &source_inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    let build_dir = repo_root.join(".fz/build");
    let static_lib = build_dir.join("libmegaserver.a");
    let object = build_dir.join("megaserver.o");
    let header = repo_root.join("include/megaserver.h");
    let abi_manifest = repo_root.join("include/megaserver.abi.json");
    let artifact_manifest = repo_root.join("include/megaserver.artifacts.json");
    for output in [
        &static_lib,
        &object,
        &header,
        &abi_manifest,
        &artifact_manifest,
    ] {
        println!("cargo:rerun-if-changed={}", output.display());
    }
    if needs_fz_build(
        &source_inputs,
        &[
            &static_lib,
            &object,
            &header,
            &abi_manifest,
            &artifact_manifest,
        ],
    ) {
        let status = Command::new("fz")
            .arg("build")
            .arg("--lib")
            .current_dir(repo_root)
            .status()
            .expect("failed to invoke `fz build` for megaserver planner");
        assert!(status.success(), "`fz build` failed for megaserver planner");
    }

    assert!(
        static_lib.exists(),
        "missing Fzy static interop library: {}",
        static_lib.display()
    );

    let include_dir = repo_root.join("include");
    println!("cargo:include={}", include_dir.display());
    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=megaserver");
}

fn collect_fzy_inputs(root: &PathBuf) -> Vec<PathBuf> {
    let mut inputs = Vec::new();
    collect_fzy_inputs_into(root, &mut inputs);
    inputs
}

fn collect_fzy_inputs_into(root: &PathBuf, inputs: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_fzy_inputs_into(&path, inputs);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("fzy") {
            inputs.push(path);
        }
    }
}

fn needs_fz_build(inputs: &[PathBuf], outputs: &[&PathBuf]) -> bool {
    if outputs.iter().any(|path| !path.exists()) {
        return true;
    }

    let newest_input = inputs
        .iter()
        .filter_map(modified_time)
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let oldest_output = outputs
        .iter()
        .filter_map(|path| modified_time(path))
        .min()
        .unwrap_or(SystemTime::UNIX_EPOCH);

    newest_input > oldest_output
}

fn modified_time(path: &PathBuf) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}
