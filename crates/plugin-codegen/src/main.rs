//! Thin entry point: discover the workspace root, then invoke
//! [`plugin_codegen::generate`].
//!
//! Invoked by `cargo run -p plugin-codegen` from `npm prebuild`, and also by
//! `src-tauri/build.rs` (transitively via the library API).

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use plugin_codegen::{OutputPaths, generate};

fn main() -> ExitCode {
    let workspace_root = match resolve_workspace_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plugin-codegen: {}", e);
            return ExitCode::from(2);
        }
    };
    let out = OutputPaths::default(&workspace_root);
    match generate(&workspace_root, &out) {
        Ok(()) => {
            eprintln!(
                "plugin-codegen: generated artifacts in {} and {}",
                out.rust_generated_dir.display(),
                out.ts_generated_dir.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", e);
            ExitCode::from(1)
        }
    }
}

/// Find the workspace root by walking upward looking for a `Cargo.toml` whose
/// content contains `[workspace]`. Falls back to `CARGO_MANIFEST_DIR/..`.
fn resolve_workspace_root() -> Result<PathBuf, String> {
    if let Ok(p) = env::var("CARGO_WORKSPACE_DIR") {
        return Ok(PathBuf::from(p));
    }
    let start = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .or_else(|_| env::current_dir().map_err(|e| e.to_string()))?;
    let mut cur: &Path = &start;
    loop {
        let cargo_toml = cur.join("Cargo.toml");
        if cargo_toml.exists() {
            if let Ok(s) = std::fs::read_to_string(&cargo_toml) {
                if s.contains("[workspace]") {
                    return Ok(cur.to_path_buf());
                }
            }
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    // Fallback: assume the codegen crate lives at <workspace>/crates/plugin-codegen.
    let crate_dir = env::var("CARGO_MANIFEST_DIR").map(PathBuf::from).map_err(|e| e.to_string())?;
    crate_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not resolve workspace root".to_string())
}
