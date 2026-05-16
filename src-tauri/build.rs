//! Tauri host build script.
//!
//! Delegates plugin manifest scanning + codegen to the workspace-shared
//! `plugin-codegen` crate so the same logic runs from either
//! `cargo build --manifest-path src-tauri/Cargo.toml` (here) or
//! `cargo run -p plugin-codegen` (invoked by `npm prebuild`).

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("src-tauri has a parent (workspace root)")
        .to_path_buf();
    let plugins_dir = workspace_root.join("plugins");

    println!("cargo:rerun-if-changed={}", plugins_dir.display());
    println!("cargo:rerun-if-changed=build.rs");

    let out = plugin_codegen::OutputPaths::default(&workspace_root);
    plugin_codegen::generate(&workspace_root, &out).unwrap_or_else(|e| {
        panic!("{e}");
    });

    tauri_build::build();
}
