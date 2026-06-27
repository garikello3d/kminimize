use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../kgather/src/main.rs");
    println!("cargo:rerun-if-changed=../../crates/gather/src/lib.rs");

    let cargo = std::env::var("CARGO").expect("CARGO env var not set");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let workspace_root = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let status = Command::new(&cargo)
        .args([
            "build",
            "--release",
            "--target",
            "x86_64-unknown-linux-musl",
            "-p",
            "kgather",
        ])
        .current_dir(&workspace_root)
        // Prevent rustflags meant for kminimize from leaking into the nested build.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .expect("failed to spawn cargo build for kgather");

    assert!(
        status.success(),
        "kgather static (musl) build failed — ensure the target is installed: \
         rustup target add x86_64-unknown-linux-musl"
    );

    let kgather_bin =
        workspace_root.join("target/x86_64-unknown-linux-musl/release/kgather");
    std::fs::copy(&kgather_bin, out_dir.join("kgather")).unwrap_or_else(|e| {
        panic!("failed to copy {} to OUT_DIR: {e}", kgather_bin.display())
    });
}
