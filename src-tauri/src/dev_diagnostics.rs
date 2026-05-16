#![allow(dead_code)]
// Round 13 ships the dev-diagnostics surface (AC-9.2). The pure resolver +
// Tauri command surface is consumed by the React shell only today; future
// work (task11 sidecar lifecycle) may reuse `resolve_expected_paths` as a
// candidate path resolver before falling back to PATH search, but that
// integration lives in task11. Suppress dead-code lints module-wide.

//! Developer diagnostics for plugin sidecar binaries.
//!
//! AC-9.2: a cold start in a dev checkout where the build has not produced
//! sidecar binaries shows a diagnostics page listing every missing binary
//! plus a hint to run the appropriate `cargo build --bin <name>` command.
//! Production builds (where the bundling pipeline already ships the
//! binaries) structurally never see this page because the diagnostic list
//! is empty when all candidates resolve.
//!
//! Pure resolver design — `diagnose(workspace_root)` is the testable
//! function, `dev_diagnostics_status` is a thin Tauri command that pins
//! the workspace root to the dev `CARGO_MANIFEST_DIR/..`. Future sidecar
//! spawn paths (task11) can reuse `resolve_expected_paths` as a candidate
//! list before falling back to system PATH lookup.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::generated::plugin_registry::PLUGINS;

/// Per-plugin presence/absence verdict for the dev-diagnostics surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SidecarBinaryStatus {
    Present,
    Missing,
}

/// One row in the dev-diagnostics report. Serialized as camelCase so the
/// React side gets `pluginId`, `commandBin`, `expectedPaths`, `status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarBinaryDiagnostic {
    pub plugin_id: String,
    pub command_bin: String,
    pub expected_paths: Vec<String>,
    pub status: SidecarBinaryStatus,
}

/// The four candidate paths a dev checkout might place a sidecar binary at.
/// We check both `src-tauri/target/{debug,release}/` (Cargo's per-package
/// target dir when `src-tauri` builds standalone) and the workspace-level
/// `target/{debug,release}/` (when the sidecar crate lives as a workspace
/// member building into the shared target).
pub fn resolve_expected_paths(workspace_root: &Path, command_bin: &str) -> Vec<PathBuf> {
    vec![
        workspace_root
            .join("src-tauri")
            .join("target")
            .join("debug")
            .join(command_bin),
        workspace_root
            .join("src-tauri")
            .join("target")
            .join("release")
            .join(command_bin),
        workspace_root
            .join("target")
            .join("debug")
            .join(command_bin),
        workspace_root
            .join("target")
            .join("release")
            .join(command_bin),
    ]
}

/// Run the diagnostic against every registered plugin under `workspace_root`.
/// Pure: only reads the filesystem to check for path existence. Never
/// panics; an inaccessible candidate path is treated as missing.
pub fn diagnose(workspace_root: &Path) -> Vec<SidecarBinaryDiagnostic> {
    PLUGINS
        .iter()
        .map(|plugin| {
            let candidates = resolve_expected_paths(workspace_root, plugin.command_bin);
            let present = candidates.iter().any(|p| p.exists());
            SidecarBinaryDiagnostic {
                plugin_id: plugin.plugin_id.to_string(),
                command_bin: plugin.command_bin.to_string(),
                expected_paths: candidates.iter().map(|p| p.display().to_string()).collect(),
                status: if present {
                    SidecarBinaryStatus::Present
                } else {
                    SidecarBinaryStatus::Missing
                },
            }
        })
        .collect()
}

/// Dev workspace root inferred from this crate's manifest dir. Matches the
/// resolver `plugin_sqlite::resolve_workspace_migrations_dir` uses, so both
/// modules agree on what "the workspace" is in a dev checkout.
pub fn workspace_root_for_dev() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri crate has a parent workspace dir")
        .to_path_buf()
}

/// Tauri command surface — never aborts bootstrap; pure read-only inspection.
#[tauri::command]
pub fn dev_diagnostics_status() -> Vec<SidecarBinaryDiagnostic> {
    diagnose(&workspace_root_for_dev())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const REAL_PLUGIN: &str = "example-notes";
    /// example-notes' command_bin per `plugins/example-notes/plugin.toml`.
    const REAL_COMMAND_BIN: &str = "notes-plugin";

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, b"#!/usr/bin/env bash\nexit 0\n").unwrap();
    }

    #[test]
    fn resolve_expected_paths_lists_four_dev_locations() {
        let root = Path::new("/tmp/fake-workspace");
        let paths = resolve_expected_paths(root, "notes-plugin");
        assert_eq!(paths.len(), 4);
        assert!(paths
            .iter()
            .any(|p| p.ends_with("src-tauri/target/debug/notes-plugin")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with("src-tauri/target/release/notes-plugin")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with("target/debug/notes-plugin")));
        assert!(paths
            .iter()
            .any(|p| p.ends_with("target/release/notes-plugin")));
    }

    #[test]
    fn diagnose_reports_missing_when_no_binary_present() {
        let dir = tmp();
        let diagnostics = diagnose(dir.path());
        // example-notes is the only bundled plugin; its sidecar binary
        // does not exist in the temp root, so its status is Missing.
        let example = diagnostics
            .iter()
            .find(|d| d.plugin_id == REAL_PLUGIN)
            .expect("example-notes present");
        assert_eq!(example.status, SidecarBinaryStatus::Missing);
        assert_eq!(example.command_bin, REAL_COMMAND_BIN);
        assert_eq!(example.expected_paths.len(), 4);
        // Every plugin in the registry should appear in the result.
        for plugin in PLUGINS {
            assert!(
                diagnostics.iter().any(|d| d.plugin_id == plugin.plugin_id),
                "missing diagnostic for {}",
                plugin.plugin_id
            );
        }
    }

    #[test]
    fn diagnose_reports_present_when_binary_at_src_tauri_target_debug() {
        let dir = tmp();
        touch(
            &dir.path()
                .join("src-tauri/target/debug")
                .join(REAL_COMMAND_BIN),
        );
        let diagnostics = diagnose(dir.path());
        let example = diagnostics
            .iter()
            .find(|d| d.plugin_id == REAL_PLUGIN)
            .unwrap();
        assert_eq!(example.status, SidecarBinaryStatus::Present);
    }

    #[test]
    fn diagnose_reports_present_when_binary_at_workspace_target_release() {
        let dir = tmp();
        touch(&dir.path().join("target/release").join(REAL_COMMAND_BIN));
        let diagnostics = diagnose(dir.path());
        let example = diagnostics
            .iter()
            .find(|d| d.plugin_id == REAL_PLUGIN)
            .unwrap();
        assert_eq!(example.status, SidecarBinaryStatus::Present);
    }

    #[test]
    fn diagnose_includes_every_registered_plugin() {
        let dir = tmp();
        let diagnostics = diagnose(dir.path());
        assert_eq!(diagnostics.len(), PLUGINS.len());
        for plugin in PLUGINS {
            assert!(
                diagnostics.iter().any(|d| d.plugin_id == plugin.plugin_id
                    && d.command_bin == plugin.command_bin),
                "diagnostics should include {} (command_bin {})",
                plugin.plugin_id,
                plugin.command_bin
            );
        }
    }

    #[test]
    fn diagnostic_serde_camelcase() {
        let d = SidecarBinaryDiagnostic {
            plugin_id: "p".into(),
            command_bin: "b".into(),
            expected_paths: vec!["/x".into()],
            status: SidecarBinaryStatus::Missing,
        };
        let v: serde_json::Value = serde_json::to_value(&d).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["commandBin", "expectedPaths", "pluginId", "status"]
        );
    }

    #[test]
    fn status_serializes_as_lowercase_literal() {
        let p: serde_json::Value = serde_json::to_value(SidecarBinaryStatus::Present).unwrap();
        assert_eq!(p, serde_json::Value::String("present".into()));
        let m: serde_json::Value = serde_json::to_value(SidecarBinaryStatus::Missing).unwrap();
        assert_eq!(m, serde_json::Value::String("missing".into()));
    }
}
