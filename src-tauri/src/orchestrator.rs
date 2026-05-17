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
use terminal_mesh_core::{ActorCommand, TerminalSpec};
use uuid::Uuid;

use crate::claude_discovery::DiscoveryCache;
use crate::dev_diagnostics::workspace_root_for_dev;
use crate::mcp_config::{self, McpConfigKind};
use crate::terminal_mesh::{spawn_into_registry, TerminalMeshRegistry};

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

    #[cfg(test)]
    pub fn record_session(&self, session: OrchestratorSession) {
        let mut guard = self.inner.lock().expect("OrchestratorState poisoned");
        *guard = Some(session);
    }

    pub fn take_session(&self) -> Option<OrchestratorSession> {
        let mut guard = self.inner.lock().expect("OrchestratorState poisoned");
        guard.take()
    }

    /// Concurrency-safe single-instance launch: holds the inner
    /// mutex across the check + spawn + record so two simultaneous
    /// callers can never both spawn `claude`. The second caller
    /// blocks on the mutex, then observes the first caller's
    /// session and returns it without invoking `spawn_fn` again.
    ///
    /// `spawn_fn` is the injectable spawn closure (config gen +
    /// `TerminalActor::spawn` + register-in-registry). On failure
    /// the lock leaves the slot empty so a future caller can retry.
    pub fn launch_locked<F>(&self, spawn_fn: F) -> Result<OrchestratorSession, OrchestratorError>
    where
        F: FnOnce() -> Result<OrchestratorSession, OrchestratorError>,
    {
        let mut guard = self.inner.lock().expect("OrchestratorState poisoned");
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        let session = spawn_fn()?;
        *guard = Some(session.clone());
        Ok(session)
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
    registry: State<'_, TerminalMeshRegistry>,
) -> Result<OrchestratorStatus, OrchestratorErrorDto> {
    Ok(resolve_status(&state, &discovery, &registry))
}

/// Pure status-resolution helper (testable without a full Tauri
/// app). Verifies registry liveness: if a recorded session points
/// at a `terminal_id` the registry no longer holds (natural exit,
/// crash, etc.), clears the stale state and falls through to the
/// discovery-based status.
pub(crate) fn resolve_status(
    state: &OrchestratorState,
    discovery: &DiscoveryCache,
    registry: &TerminalMeshRegistry,
) -> OrchestratorStatus {
    if let Some(session) = state.snapshot() {
        if registry.contains(session.terminal_id) {
            return OrchestratorStatus::Ready { session };
        }
        // Stale: the actor naturally exited and the forwarder
        // evicted the registry entry. Clear our state too so the
        // next launch path rotates cleanly.
        let _ = state.take_session();
    }
    match discovery.snapshot() {
        Some(Ok(_)) => OrchestratorStatus::NotLaunched,
        Some(Err(_)) | None => OrchestratorStatus::ClaudeMissing,
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
    // Require claude discovery before holding the launch mutex so
    // the missing-claude path doesn't serialize. The `launch_locked`
    // helper below handles the concurrency case (two callers race
    // → exactly one spawn) by holding the mutex across the entire
    // check + spawn + record sequence.
    let record = match discovery.snapshot() {
        Some(Ok(r)) => r,
        _ => return Err(OrchestratorErrorDto::from(&OrchestratorError::ClaudeMissing)),
    };
    let claude_path = record.path;
    let agent_platform_root = bootstrap.agent_platform_root.clone();
    let app_data_root = bootstrap.app_data_root.clone();

    let session = state
        .launch_locked(|| {
            spawn_orchestrator_claude(
                &app,
                &registry,
                &agent_platform_root,
                &app_data_root,
                &claude_path,
            )
        })
        .map_err(|e| OrchestratorErrorDto::from(&e))?;
    Ok(OrchestratorStatus::Ready { session })
}

/// Generate the orchestrator-variant MCP config + spawn the claude
/// PTY + register it in the terminal-mesh registry. Returns the
/// session record on success. On `SpawnFailed` after the config was
/// written, the just-written config is deleted so the next attempt
/// starts clean.
pub(crate) fn spawn_orchestrator_claude(
    app: &AppHandle,
    registry: &TerminalMeshRegistry,
    agent_platform_root: &std::path::Path,
    app_data_root: &std::path::Path,
    claude_path: &std::path::Path,
) -> Result<OrchestratorSession, OrchestratorError> {
    let tab_id = Uuid::new_v4().to_string();
    let workspace_root_for_dev_path = workspace_root_for_dev();
    let doc = mcp_config::generate_config(
        &tab_id,
        agent_platform_root,
        McpConfigKind::Orchestrator,
        app_data_root,
        &workspace_root_for_dev_path,
    )
    .map_err(|e| OrchestratorError::McpConfigFailed {
        message: e.to_string(),
    })?;
    let mcp_config_path =
        mcp_config::write_atomic(app_data_root, &tab_id, &doc).map_err(|e| {
            OrchestratorError::McpConfigFailed {
                message: e.to_string(),
            }
        })?;

    let spec = TerminalSpec {
        terminal_id: Uuid::new_v4(),
        command: claude_path.to_path_buf(),
        args: vec![
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            mcp_config_path.display().to_string(),
        ],
        cwd: Some(agent_platform_root.to_path_buf()),
        env: vec![(
            "APP_DATA_DIR".into(),
            app_data_root.display().to_string(),
        )],
        cols: 100,
        rows: 30,
    };

    match spawn_into_registry(spec, app, registry) {
        Ok(terminal_id) => Ok(OrchestratorSession {
            terminal_id,
            tab_id,
            mcp_config_path,
        }),
        Err(e) => {
            // Clean up the just-written MCP config so the next
            // attempt doesn't trip the startup_gc / orphan-config
            // checks later.
            mcp_config::delete_config(app_data_root, &tab_id);
            Err(OrchestratorError::SpawnFailed {
                message: e.to_string(),
            })
        }
    }
}

#[tauri::command]
pub fn orchestrator_shutdown(
    state: State<'_, OrchestratorState>,
    registry: State<'_, TerminalMeshRegistry>,
    bootstrap: State<'_, OrchestratorBootstrap>,
) -> Result<(), OrchestratorErrorDto> {
    shutdown_session(&state, &registry, &bootstrap.app_data_root);
    Ok(())
}

/// Best-effort orchestrator session teardown. Idempotent: missing
/// session, missing registry entry, and missing config file are all
/// treated as success. Logged-but-ignored failures only.
pub(crate) fn shutdown_session(
    state: &OrchestratorState,
    registry: &TerminalMeshRegistry,
    app_data_root: &std::path::Path,
) {
    let Some(session) = state.take_session() else {
        return;
    };
    // 1. Send the actor a graceful Shutdown via its command channel
    //    (best-effort; if the actor is already dead the send fails).
    if let Some(tx) = registry.lookup_command_tx(session.terminal_id) {
        // tokio::mpsc::Sender::try_send is non-blocking; the Shutdown
        // signal can be lost only if the channel is full or already
        // closed (actor already exited). Both are acceptable: the
        // forwarder loop will see the channel close and the eventual
        // registry eviction handles the lifetime cleanup.
        let _ = tx.try_send(ActorCommand::Shutdown);
    }
    // 2. Evict the registry entry so a subsequent launch rotates
    //    cleanly without a stale terminal_id collision check.
    registry.forget(session.terminal_id);
    // 3. Delete the per-tab MCP config; missing file is acceptable.
    mcp_config::delete_config(app_data_root, &session.tab_id);
    tracing::info!(
        tab_id = %session.tab_id,
        "orchestrator session shutdown complete"
    );
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

    // ----- Round 36 -----

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 4 OS threads race into `launch_locked` with a spawn closure
    /// that atomically increments a shared counter. After all join,
    /// the counter MUST equal 1 (single-instance enforcement) and
    /// every caller MUST observe the same session's identifiers.
    #[test]
    fn launch_locked_runs_spawn_closure_exactly_once_under_concurrent_callers() {
        let state = Arc::new(OrchestratorState::new());
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let canonical_session = OrchestratorSession {
            terminal_id: Uuid::new_v4(),
            tab_id: "tab-race".into(),
            mcp_config_path: PathBuf::from("/tmp/race.json"),
        };

        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = state.clone();
            let counter = spawn_count.clone();
            let session = canonical_session.clone();
            handles.push(std::thread::spawn(move || {
                s.launch_locked(|| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(session.clone())
                })
                .expect("launch_locked returned an error")
            }));
        }

        let results: Vec<OrchestratorSession> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            1,
            "spawn closure must run exactly once across concurrent callers"
        );
        for r in &results {
            assert_eq!(r.terminal_id, canonical_session.terminal_id);
            assert_eq!(r.tab_id, canonical_session.tab_id);
            assert_eq!(r.mcp_config_path, canonical_session.mcp_config_path);
        }
    }

    /// After shutdown, a fresh `launch_locked` MUST invoke the spawn
    /// closure again and return rotated identifiers — the close /
    /// reopen lifecycle. Sequencing this in a single-threaded test
    /// is sufficient; concurrency is covered above.
    #[test]
    fn relaunch_after_shutdown_rotates_session_identifiers() {
        let state = OrchestratorState::new();
        let counter = AtomicUsize::new(0);

        let first_session_id = Uuid::new_v4();
        let s1 = state
            .launch_locked(|| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(OrchestratorSession {
                    terminal_id: first_session_id,
                    tab_id: "tab-1".into(),
                    mcp_config_path: PathBuf::from("/tmp/1.json"),
                })
            })
            .expect("first launch ok");
        assert_eq!(s1.tab_id, "tab-1");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Simulated shutdown: clear the recorded session.
        let _ = state.take_session();
        assert!(state.snapshot().is_none());

        let second_session_id = Uuid::new_v4();
        let s2 = state
            .launch_locked(|| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(OrchestratorSession {
                    terminal_id: second_session_id,
                    tab_id: "tab-2".into(),
                    mcp_config_path: PathBuf::from("/tmp/2.json"),
                })
            })
            .expect("second launch ok");

        assert_eq!(s2.tab_id, "tab-2");
        assert_ne!(s2.terminal_id, s1.terminal_id);
        assert_ne!(s2.mcp_config_path, s1.mcp_config_path);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "shutdown must un-cache so the second launch runs the spawn closure again"
        );
    }

    /// `shutdown_session` MUST clear the recorded state and delete
    /// the per-tab MCP config file from disk. Missing registry entry
    /// and missing file are both acceptable (the actor may have
    /// already exited; the config may have already been GC'd).
    #[test]
    fn shutdown_clears_state_and_deletes_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_data_root = tmp.path().to_path_buf();
        let configs_dir = app_data_root.join("claude-mcp-configs");
        std::fs::create_dir_all(&configs_dir).expect("mkdir configs");
        let tab_id = "tab-shutdown-test";
        let config_path = configs_dir.join(format!("{tab_id}.json"));
        std::fs::write(&config_path, b"{}").expect("write dummy config");
        assert!(config_path.exists());

        let state = OrchestratorState::new();
        state.record_session(OrchestratorSession {
            terminal_id: Uuid::new_v4(),
            tab_id: tab_id.into(),
            mcp_config_path: config_path.clone(),
        });
        assert!(state.snapshot().is_some());

        let registry = TerminalMeshRegistry::new();
        // No registered entry — the lookup_command_tx + forget paths
        // must be tolerant of this.

        shutdown_session(&state, &registry, &app_data_root);

        assert!(state.snapshot().is_none(), "state cleared");
        assert!(!config_path.exists(), "config file deleted");
    }

    /// Shutdown MUST be idempotent: calling it with no recorded
    /// session is a no-op success.
    #[test]
    fn shutdown_with_no_session_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = OrchestratorState::new();
        let registry = TerminalMeshRegistry::new();
        shutdown_session(&state, &registry, tmp.path());
        assert!(state.snapshot().is_none());
    }

    /// `resolve_status` MUST treat a recorded session whose
    /// `terminal_id` is no longer in the registry as stale: clear
    /// `OrchestratorState` and fall through to the discovery-based
    /// status. Without this, a naturally-exited claude would leave
    /// the orchestrator UI stuck in `Ready` forever.
    #[test]
    fn status_clears_stale_session_when_registry_no_longer_contains_terminal() {
        let state = OrchestratorState::new();
        let stale_id = Uuid::new_v4();
        state.record_session(OrchestratorSession {
            terminal_id: stale_id,
            tab_id: "tab-stale".into(),
            mcp_config_path: PathBuf::from("/tmp/stale.json"),
        });
        let registry = TerminalMeshRegistry::new(); // empty — stale_id not registered
        let discovery = DiscoveryCache::empty();
        discovery.store(Ok(crate::claude_discovery::ClaudePathRecord {
            path: PathBuf::from("/usr/local/bin/claude"),
            version: Some("1.2.3".into()),
            discovered_at: "2026-05-17T00:00:00Z".into(),
        }));

        let status = resolve_status(&state, &discovery, &registry);

        assert!(
            matches!(status, OrchestratorStatus::NotLaunched),
            "stale session must clear and fall through to NotLaunched, got {status:?}",
        );
        assert!(
            state.snapshot().is_none(),
            "stale state must be cleared during resolve_status",
        );
    }

    /// Liveness happy path: a recorded session whose terminal_id
    /// IS in the registry stays `Ready`.
    #[test]
    fn status_returns_ready_when_registry_still_contains_terminal() {
        let state = OrchestratorState::new();
        let live_id = Uuid::new_v4();
        state.record_session(OrchestratorSession {
            terminal_id: live_id,
            tab_id: "tab-live".into(),
            mcp_config_path: PathBuf::from("/tmp/live.json"),
        });
        let registry = TerminalMeshRegistry::new();
        // Insert via the internal path: tests in this module are in
        // the same crate, so we can hand-construct a fake entry via
        // a no-op channel + scrollback for the contains() check.
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        let scrollback = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        registry.record(live_id, command_tx, scrollback);

        let discovery = DiscoveryCache::empty();
        let status = resolve_status(&state, &discovery, &registry);

        assert!(matches!(status, OrchestratorStatus::Ready { .. }));
        assert!(state.snapshot().is_some(), "live state preserved");
    }
}
