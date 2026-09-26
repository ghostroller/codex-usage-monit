use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    // A source identity, not a timestamp or target-specific binary hash. The
    // same checkout must identify itself equally on Windows, Linux and macOS.
    let mut files = vec![
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
        PathBuf::from("build.rs"),
        PathBuf::from("src/remote_agent_manager/release_bootstrap.py"),
        PathBuf::from("src/remote_agent_manager/release_bootstrap.ps1"),
    ];
    let mut directories = vec![PathBuf::from("src")];
    while let Some(directory) = directories.pop() {
        println!("cargo:rerun-if-changed={}", directory.display());
        for entry in fs::read_dir(directory).expect("read source tree") {
            let entry = entry.expect("read source entry");
            if entry.file_type().expect("source file type").is_dir() {
                directories.push(entry.path());
            } else if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "rs")
            {
                files.push(entry.path());
            }
        }
    }
    files.sort_by_key(|path| path.to_string_lossy().replace('\\', "/"));
    let mut hash = Sha256::new();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.to_string_lossy().replace('\\', "/");
        let text = fs::read_to_string(&path)
            .expect("read UTF-8 build input")
            .replace("\r\n", "\n");
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((text.len() as u64).to_le_bytes());
        hash.update(text.as_bytes());
    }
    let build_id = format!("{:x}", hash.finalize());
    println!("cargo:rustc-env=MONIT_BUILD_ID={build_id}");
    println!(
        "cargo:rustc-env=MONIT_BUILD_TARGET={}",
        env::var("TARGET").expect("build target")
    );
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        build_recorder_host(&build_id);
    }
}

fn build_recorder_host(build_id: &str) {
    let target = env::var("TARGET").expect("build target");
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("build output")).join("recorder-host.exe");
    let mut compiler = Command::new(env::var_os("RUSTC").expect("Rust compiler"));
    compiler
        .args([
            "--edition=2024",
            "--crate-name",
            "codex_usage_monit_recorder_host",
        ])
        .arg("src/service/recorder_host_main.rs")
        .args([
            "--target",
            &target,
            "-C",
            "opt-level=s",
            "-C",
            "debuginfo=0",
        ])
        .arg("-o")
        .arg(&output)
        .env("MONIT_BUILD_ID", build_id)
        .env("MONIT_BUILD_TARGET", &target)
        .env("CARGO_PKG_VERSION", env::var("CARGO_PKG_VERSION").unwrap());
    // Cargo provides this for a configured cross/native linker. The helper is
    // std-only so it has no dependency ordering or nested Cargo invocation.
    if let Some(linker) = env::var_os("RUSTC_LINKER") {
        compiler
            .arg("-C")
            .arg(format!("linker={}", linker.to_string_lossy()));
    }
    let result = compiler
        .output()
        .expect("compile embedded Windows recorder host");
    assert!(
        result.status.success(),
        "embedded Windows recorder host compilation failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
