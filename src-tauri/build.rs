//! Tauri host build script.
//!
//! Delegates plugin manifest scanning + codegen to the workspace-shared
//! `plugin-codegen` crate so the same logic runs from either
//! `cargo build --manifest-path src-tauri/Cargo.toml` (here) or
//! `cargo run -p plugin-codegen` (invoked by `npm prebuild`).
//!
//! Also stages target-triple-suffixed copies of every sidecar binary
//! listed in `tauri.conf.json::bundle.externalBin` so `tauri_build::build()`
//! does not fail when the host is compiled before the sidecars have
//! been built into the expected `<name>-<triple>` form.

use std::path::PathBuf;

const BUNDLED_SIDECARS: &[&str] = &[
    "terminal-mesh-sidecar",
    "notes-plugin",
    "papers-plugin",
];

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("src-tauri has a parent (workspace root)")
        .to_path_buf();
    let plugins_dir = workspace_root.join("plugins");

    println!("cargo:rerun-if-changed={}", plugins_dir.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=tauri.conf.json");

    let out = plugin_codegen::OutputPaths::default(&workspace_root);
    plugin_codegen::generate(&workspace_root, &out).unwrap_or_else(|e| {
        panic!("{e}");
    });

    // Stage `<sidecar>-<triple>` placeholders so tauri_build's
    // externalBin check passes even when the sidecars have not been
    // built. This is a build-time concern only; the real binaries are
    // produced by `npm run build:sidecars` before the bundler actually
    // copies them, and at runtime the host resolves binaries via
    // `dev_diagnostics::resolve_expected_paths` (which looks at the
    // canonical un-suffixed name).
    let target_triple = std::env::var("TARGET").unwrap_or_default();
    if !target_triple.is_empty() {
        for profile in ["debug", "release"] {
            let target_dir = workspace_root.join("target").join(profile);
            if !target_dir.is_dir() {
                let _ = std::fs::create_dir_all(&target_dir);
            }
            for sidecar in BUNDLED_SIDECARS {
                let canonical = target_dir.join(sidecar);
                let suffixed = target_dir.join(format!("{sidecar}-{target_triple}"));
                if suffixed.exists() {
                    continue;
                }
                if canonical.exists() {
                    let _ = std::fs::copy(&canonical, &suffixed);
                } else {
                    // No real binary yet — create a zero-byte placeholder
                    // so the bundler check passes during cargo check /
                    // cargo test of the host crate. The real binary is
                    // produced by `npm run build:sidecars` before any
                    // packaged build runs.
                    let _ = std::fs::File::create(&suffixed);
                }
            }
        }
    }

    tauri_build::build();
}
