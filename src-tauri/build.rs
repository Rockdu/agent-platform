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

    // Stage `<sidecar>-<triple>` artefacts so tauri_build's
    // `bundle.externalBin` check finds the expected name.
    //
    // `tauri.conf.json::bundle.externalBin` hardcodes
    // `../target/release/<sidecar>` (the path is NOT profile-templated)
    // and `tauri_build::build()` validates that path on EVERY cargo
    // profile, including debug. So the staging destination must be
    // `target/release/<sidecar>-<triple>` regardless of profile —
    // staging under `target/<profile>/` was a bug that made a clean
    // debug `cargo check` fail with
    // `resource path ../target/release/<sidecar>-<triple> doesn't exist`
    // before any compilation began.
    //
    // The profile-dependent behaviour is preserved for strictness:
    //
    // - debug: zero-byte placeholders are acceptable. Developers run
    //   `cargo check` / `cargo test` against the host crate without
    //   pre-building sidecars. A zero-byte file at the externalBin
    //   path satisfies Tauri's validation without packaging a debug
    //   bundle (debug never builds bundles in this project).
    //
    // - release: zero-byte placeholders are REFUSED. A release build
    //   that ships a bundle MUST package real sidecars; otherwise
    //   the packaged app would crash the moment it spawns the
    //   sidecar. Panic with a clear message pointing at the
    //   `npm run build:sidecars:release` script.
    let target_triple = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if !target_triple.is_empty() {
        let target_dir = workspace_root.join("target").join("release");
        if !target_dir.is_dir() {
            let _ = std::fs::create_dir_all(&target_dir);
        }
        // Cargo emits `.exe` for any Windows target triple (msvc/gnu/uwp);
        // Tauri's `bundle.externalBin` validation expects the staged
        // name to carry the same suffix, so omitting it makes both the
        // source-lookup and the staged-name lookup miss real binaries
        // and panic after a successful `cargo build --release`.
        let target_exe_suffix = if target_triple.contains("windows") {
            ".exe"
        } else {
            ""
        };
        for sidecar in BUNDLED_SIDECARS {
            let canonical = target_dir.join(format!("{sidecar}{target_exe_suffix}"));
            let suffixed =
                target_dir.join(format!("{sidecar}-{target_triple}{target_exe_suffix}"));
            let needs_real = profile == "release";
            // If the suffixed file already exists, validate it for release.
            if suffixed.exists() {
                if needs_real {
                    let meta = std::fs::metadata(&suffixed);
                    let is_empty = meta.map(|m| m.len() == 0).unwrap_or(true);
                    if is_empty {
                        panic!(
                            "release bundle would ship zero-byte sidecar `{}`. Run `npm run build:sidecars:release` to produce a real binary before `cargo build --release`.",
                            suffixed.display()
                        );
                    }
                }
                continue;
            }
            if canonical.exists() {
                let meta = std::fs::metadata(&canonical).ok();
                let is_real = meta.map(|m| m.len() > 0).unwrap_or(false);
                if is_real {
                    let _ = std::fs::copy(&canonical, &suffixed);
                    continue;
                }
            }
            // Canonical missing or zero-byte.
            if needs_real {
                panic!(
                    "release bundle requires `{}`. Run `npm run build:sidecars:release` to produce real sidecars before `cargo build --release`.",
                    suffixed.display()
                );
            }
            // Debug profile: zero-byte placeholder is acceptable so
            // `cargo check` / `cargo test` against the host don't
            // need sidecars pre-built.
            let _ = std::fs::File::create(&suffixed);
        }
    }

    tauri_build::build();
}
