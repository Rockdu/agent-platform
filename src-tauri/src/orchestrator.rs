//! Orchestrator host-side tab (AC-3.1 + AC-3.2).
//!
//! Single-instance privileged tab fixed top-left of the strip. On
//! open, auto-launches `claude --strict-mcp-config --mcp-config
//! <generated.json>` against an orchestrator-variant MCP config
//! (the one with `--cross-tab-read` on `terminal-mesh`). When
//! `claude` is missing, the frontend falls back to the existing
//! task13 onboarding card.
//!
//! Single-instance is structurally enforced at the frontend (the
//! tab strip renders exactly one orchestrator slot) AND at the
//! backend (`OrchestratorState` holds at most one
//! `OrchestratorSession`). The `orchestrator_launch_claude` command
//! is idempotent — a second call returns the same session.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use terminal_mesh_core::TerminalSpec;
use uuid::Uuid;

use crate::claude_discovery::DiscoveryCache;
use crate::dev_diagnostics::workspace_root_for_dev;
use crate::mcp_config::{self, McpConfigKind};
use crate::terminal_mesh::{spawn_into_registry, TerminalMeshError, TerminalMeshRegistry};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorSession {
    pub terminal_id: Uuid,
    pub tab_id: String,
    pub mcp_config_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum OrchestratorStatus {
    /// `claude` is launched and the terminal_id is live.
    Ready { session: OrchestratorSession },
    /// `claude` discovery succeeded but the user hasn't activated
    /// the orchestrator yet (frontend should call
    /// `orchestrator_launch_claude` to spawn).
    NotLaunched,
    /// `claude` discovery returned `ClaudeNotFound`; the frontend
    /// should keep the existing onboarding card visible.
    ClaudeMissing,
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("claude binary not discovered; user must run claude discovery first")]
    ClaudeMissing,

    #[error("MCP config generation failed: {message}")]
    McpConfigFailed { message: String },

    #[error("terminal spawn failed: {message}")]
    SpawnFailed { message: String },

    /// Reserved for future io error paths (e.g., explicit fs
    /// operations during shutdown coordination in task38). Kept now
    /// so the wire DTO can be exhaustively switched on by the
    /// frontend without a future-breaking refactor.
    #[allow(dead_code)]
    #[error("io error in `{context}`: {message}")]
    Io { context: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum OrchestratorErrorDto {
    ClaudeMissing,
    McpConfigFailed { message: String },
    SpawnFailed { message: String },
    Io { context: String, message: String },
}

impl From<&OrchestratorError> for OrchestratorErrorDto {
    fn from(err: &OrchestratorError) -> Self {
        match err {
            OrchestratorError::ClaudeMissing => Self::ClaudeMissing,
            OrchestratorError::McpConfigFailed { message } => Self::McpConfigFailed {
                message: message.clone(),
            },
            OrchestratorError::SpawnFailed { message } => Self::SpawnFailed {
                message: message.clone(),
            },
            OrchestratorError::Io { context, message } => Self::Io {
                context: context.clone(),
                message: message.clone(),
            },
        }
    }
}

pub struct OrchestratorState {
    inner: Mutex<Option<OrchestratorSession>>,
}

impl Default for OrchestratorState {
    fn default() -> Self {
        Self::new()
    }
}

impl OrchestratorState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> Option<OrchestratorSession> {
        let guard = self.inner.lock().expect("OrchestratorState poisoned");
        guard.clone()
    }

    pub fn record_session(&self, session: OrchestratorSession) {
        let mut guard = self.inner.lock().expect("OrchestratorState poisoned");
        *guard = Some(session);
    }

    pub fn take_session(&self) -> Option<OrchestratorSession> {
        let mut guard = self.inner.lock().expect("OrchestratorState poisoned");
        guard.take()
    }
}

/// Tauri-managed bootstrap context for the orchestrator. Stores the
/// resolved `~/AgentPlatform/` root so the spawn path can `cd` into
/// the user-visible workspaces root by default. The orchestrator's
/// MCP config also passes this as the `--workspace` argv.
pub struct OrchestratorBootstrap {
    pub agent_platform_root: PathBuf,
    pub app_data_root: PathBuf,
}

#[tauri::command]
pub fn orchestrator_status(
    state: State<'_, OrchestratorState>,
    discovery: State<'_, DiscoveryCache>,
) -> Result<OrchestratorStatus, OrchestratorErrorDto> {
    if let Some(session) = state.snapshot() {
        return Ok(OrchestratorStatus::Ready { session });
    }
    // Inspect discovery cache: ready → NotLaunched; not-found →
    // ClaudeMissing (frontend falls back to the onboarding card).
    match discovery.snapshot() {
        Some(Ok(_)) => Ok(OrchestratorStatus::NotLaunched),
        Some(Err(_)) => Ok(OrchestratorStatus::ClaudeMissing),
        None => Ok(OrchestratorStatus::ClaudeMissing),
    }
}

#[tauri::command]
pub fn orchestrator_launch_claude(
    app: AppHandle,
    state: State<'_, OrchestratorState>,
    discovery: State<'_, DiscoveryCache>,
    registry: State<'_, TerminalMeshRegistry>,
    bootstrap: State<'_, OrchestratorBootstrap>,
) -> Result<OrchestratorStatus, OrchestratorErrorDto> {
    // Idempotent fast path: if a session is already recorded, return
    // it without re-spawning. This satisfies AC-3.1 "opening twice
    // never spawns two tabs" at the backend layer.
    if let Some(session) = state.snapshot() {
        return Ok(OrchestratorStatus::Ready { session });
    }

    // Require claude discovery to have resolved a path.
    let record = match discovery.snapshot() {
        Some(Ok(r)) => r,
        _ => return Err(OrchestratorErrorDto::from(&OrchestratorError::ClaudeMissing)),
    };
    let claude_path = record.path;

    // Generate the orchestrator-variant MCP config (--cross-tab-read
    // on the terminal-mesh entry; per `docs/specs/claude-launch.md`
    // §"Orchestrator's Special MCP Config"). The tab id is a fresh
    // UUID; the workspace path is the AgentPlatform root so the
    // orchestrator's claude session lives where the user's
    // workspaces live.
    let tab_id = Uuid::new_v4().to_string();
    let workspace_root_for_dev_path = workspace_root_for_dev();
    let doc = mcp_config::generate_config(
        &tab_id,
        &bootstrap.agent_platform_root,
        McpConfigKind::Orchestrator,
        &bootstrap.app_data_root,
        &workspace_root_for_dev_path,
    )
    .map_err(|e| OrchestratorErrorDto::from(&OrchestratorError::McpConfigFailed {
        message: e.to_string(),
    }))?;
    let mcp_config_path = mcp_config::write_atomic(&bootstrap.app_data_root, &tab_id, &doc)
        .map_err(|e| OrchestratorErrorDto::from(&OrchestratorError::McpConfigFailed {
            message: e.to_string(),
        }))?;

    // Spawn `claude --strict-mcp-config --mcp-config <path>` with cwd
    // = AgentPlatform root and APP_DATA_DIR env (sidecars expect it).
    let spec = TerminalSpec {
        terminal_id: Uuid::new_v4(),
        command: claude_path,
        args: vec![
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            mcp_config_path.display().to_string(),
        ],
        cwd: Some(bootstrap.agent_platform_root.clone()),
        env: vec![(
            "APP_DATA_DIR".into(),
            bootstrap.app_data_root.display().to_string(),
        )],
        cols: 100,
        rows: 30,
    };

    let terminal_id =
        spawn_into_registry(spec, &app, &registry).map_err(|e: TerminalMeshError| {
            OrchestratorErrorDto::from(&OrchestratorError::SpawnFailed {
                message: e.to_string(),
            })
        })?;

    let session = OrchestratorSession {
        terminal_id,
        tab_id,
        mcp_config_path,
    };
    state.record_session(session.clone());
    Ok(OrchestratorStatus::Ready { session })
}

#[tauri::command]
pub fn orchestrator_shutdown(
    state: State<'_, OrchestratorState>,
) -> Result<(), OrchestratorErrorDto> {
    // MVP: just forget the session. The actual `ActorCommand::
    // Shutdown` for the underlying claude PTY is handled by the
    // existing `terminal_shutdown` Tauri command path when the user
    // (or future task38 quit sequencing) closes the terminal.
    // Reserved for task38 to wire the full kill chain.
    let _ = state.take_session();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let s = OrchestratorState::new();
        assert!(s.snapshot().is_none());
        let session = OrchestratorSession {
            terminal_id: Uuid::new_v4(),
            tab_id: "tab-abc".into(),
            mcp_config_path: PathBuf::from("/tmp/orch.json"),
        };
        s.record_session(session.clone());
        let snap = s.snapshot().expect("present after record");
        assert_eq!(snap.tab_id, session.tab_id);
        assert_eq!(snap.terminal_id, session.terminal_id);
        let taken = s.take_session().expect("taken once");
        assert_eq!(taken.tab_id, session.tab_id);
        assert!(s.snapshot().is_none());
    }

    #[test]
    fn take_session_returns_none_when_empty() {
        let s = OrchestratorState::new();
        assert!(s.take_session().is_none());
    }

    #[test]
    fn status_dto_serializes_with_kind_discriminant() {
        let s = OrchestratorStatus::Ready {
            session: OrchestratorSession {
                terminal_id: Uuid::nil(),
                tab_id: "t".into(),
                mcp_config_path: PathBuf::from("/tmp/x"),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("ready"));
        assert!(v.get("session").is_some());

        let n: serde_json::Value =
            serde_json::to_value(&OrchestratorStatus::NotLaunched).unwrap();
        assert_eq!(n.get("kind").and_then(|x| x.as_str()), Some("notLaunched"));

        let m: serde_json::Value =
            serde_json::to_value(&OrchestratorStatus::ClaudeMissing).unwrap();
        assert_eq!(m.get("kind").and_then(|x| x.as_str()), Some("claudeMissing"));
    }

    #[test]
    fn error_dto_serializes_with_kind_discriminant() {
        let dto = OrchestratorErrorDto::from(&OrchestratorError::McpConfigFailed {
            message: "boom".into(),
        });
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("mcpConfigFailed"));
        assert_eq!(v.get("message").and_then(|x| x.as_str()), Some("boom"));
    }
}
