//! First-run filesystem bootstrap.
//!
//! Creates the directory layout that the rest of the platform assumes exists.
//! Idempotent: re-running on a populated layout is a no-op.
//!
//! Layout:
//!   - ~/AgentPlatform/                       user-visible workspaces root
//!   - ~/AgentPlatform/workspaces/            per-tab workspace dirs
//!   - ${APP_DATA}/plugins/                   per-plugin SQLite + state root
//!   - ${APP_DATA}/plugins/<plugin_id>/       per-plugin subdir for each
//!                                            bundled plugin (the host
//!                                            iterates the generated registry)
//!   - ${APP_DATA}/logs/                      per-process sidecar logs
//!   - ${APP_DATA}/claude-mcp-configs/        per-tab MCP config files
//!
//! On macOS, `${APP_DATA}` resolves to
//! `~/Library/Application Support/com.agentplatform.app/` via the
//! `directories::ProjectDirs` lookup keyed by qualifier/organization/app.

use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct BootstrapPaths {
    pub agent_platform: PathBuf,
    pub workspaces: PathBuf,
    pub plugins_root: PathBuf,
    pub logs: PathBuf,
    pub claude_mcp_configs: PathBuf,
}

#[derive(Debug, thiserror::Error, Serialize)]
pub enum BootstrapError {
    #[error("user home directory could not be resolved")]
    NoHomeDir,

    #[error("application data directory could not be resolved (qualifier=com, org=agentplatform, app=app)")]
    NoAppDataDir,

    #[error("failed to create directory `{path}`: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        #[serde(skip)]
        source: std::io::Error,
    },
}

/// Resolve all required paths without touching the filesystem.
fn resolve_paths() -> Result<BootstrapPaths, BootstrapError> {
    let base = BaseDirs::new().ok_or(BootstrapError::NoHomeDir)?;
    let project = ProjectDirs::from("com", "agentplatform", "app")
        .ok_or(BootstrapError::NoAppDataDir)?;

    let agent_platform = base.home_dir().join("AgentPlatform");
    let workspaces = agent_platform.join("workspaces");
    let app_data = project.data_dir().to_path_buf();
    let plugins_root = app_data.join("plugins");
    let logs = app_data.join("logs");
    let claude_mcp_configs = app_data.join("claude-mcp-configs");

    Ok(BootstrapPaths {
        agent_platform,
        workspaces,
        plugins_root,
        logs,
        claude_mcp_configs,
    })
}

fn ensure_dir(path: &PathBuf) -> Result<(), BootstrapError> {
    std::fs::create_dir_all(path).map_err(|source| BootstrapError::CreateDir {
        path: path.clone(),
        source,
    })
}

/// Idempotently create all required directories. Returns resolved paths on success.
///
/// Stops at the first permission/IO failure and reports the offending path via
/// `BootstrapError::CreateDir`. Callers (frontend) render a Chinese error card
/// citing the offending path.
pub fn ensure_dirs() -> Result<BootstrapPaths, BootstrapError> {
    let paths = resolve_paths()?;

    for path in [
        &paths.agent_platform,
        &paths.workspaces,
        &paths.plugins_root,
        &paths.logs,
        &paths.claude_mcp_configs,
    ] {
        ensure_dir(path)?;
    }

    Ok(paths)
}

/// Create `${APP_DATA}/plugins/<plugin_id>/` for each bundled plugin id.
///
/// Designed to be called after [`ensure_dirs`] succeeds, iterating the
/// `plugin_codegen`-generated `PLUGINS` registry. Returns the per-plugin paths
/// on success; reports the first failure with the offending path so the
/// frontend can render it via `BootstrapErrorDto`.
pub fn ensure_plugin_dirs(
    plugins_root: &Path,
    plugin_ids: impl IntoIterator<Item = &'static str>,
) -> Result<Vec<PathBuf>, BootstrapError> {
    let mut paths = Vec::new();
    for id in plugin_ids {
        let dir = plugins_root.join(id);
        ensure_dir(&dir)?;
        paths.push(dir);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths_without_touching_disk() {
        let paths = resolve_paths().expect("paths resolve in test env");
        assert!(paths.agent_platform.ends_with("AgentPlatform"));
        assert!(paths.workspaces.ends_with("workspaces"));
        assert!(paths.plugins_root.ends_with("plugins"));
        assert!(paths.logs.ends_with("logs"));
        assert!(paths.claude_mcp_configs.ends_with("claude-mcp-configs"));
    }

    #[test]
    fn ensure_plugin_dirs_creates_subdirs_under_temp_root() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let plugins_root = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins_root).unwrap();

        let created = ensure_plugin_dirs(&plugins_root, ["alpha", "beta"]).expect("create plugin dirs");
        assert_eq!(created.len(), 2);
        assert!(plugins_root.join("alpha").is_dir());
        assert!(plugins_root.join("beta").is_dir());
    }

    #[test]
    fn ensure_plugin_dirs_reports_offending_path_on_failure() {
        // Pointing the plugin-root at a path whose parent doesn't exist is the
        // most reliable cross-platform way to force CreateDir to fail.
        let unwritable_root = PathBuf::from("/nonexistent-root-for-test-9b3e7f/plugins");
        let err = ensure_plugin_dirs(&unwritable_root, ["alpha"])
            .expect_err("creation must fail with no parent dir");
        match err {
            BootstrapError::CreateDir { path, .. } => {
                assert!(path.ends_with("alpha"));
            }
            other => panic!("unexpected error variant: {other}"),
        }
    }
}
