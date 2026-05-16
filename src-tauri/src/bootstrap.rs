//! First-run filesystem bootstrap.
//!
//! Creates the directory layout that the rest of the platform assumes exists.
//! Idempotent: re-running on a populated layout is a no-op.
//!
//! Layout (per docs/plan.md AC-9.1):
//!   - ~/AgentPlatform/                       user-visible workspaces root
//!   - ~/AgentPlatform/workspaces/            per-tab workspace dirs
//!   - ${APP_DATA}/plugins/                   per-plugin SQLite + state
//!   - ${APP_DATA}/logs/                      per-process sidecar logs
//!   - ${APP_DATA}/claude-mcp-configs/        per-tab MCP config files
//!
//! On macOS, `${APP_DATA}` resolves to
//! `~/Library/Application Support/com.agentplatform.app/` via the
//! `directories::ProjectDirs` lookup keyed by qualifier/organization/app.

use std::path::PathBuf;

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
}
