use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

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
    println!("cargo:rustc-env=MONIT_BUILD_ID={:x}", hash.finalize());
    println!(
        "cargo:rustc-env=MONIT_BUILD_TARGET={}",
        env::var("TARGET").expect("build target")
    );
}
