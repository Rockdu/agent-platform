//! task21 / AC-3.3 host RPC back-channel for MCP sidecars.
//!
//! Unix domain socket JSON-RPC server that sidecar processes connect to
//! when they need to call back into host state. Today the only method
//! is `terminalMesh.readScrollback` (the orchestrator's terminal-mesh
//! MCP sidecar uses it to satisfy `terminal_mesh.read_scrollback(tab_id=X)`
//! from claude). Future tasks (notifications, write-confirm flow) reuse
//! the same socket / framing for additional methods.
//!
//! Authorization model:
//! - The sidecar identifies itself via `clientId` (the
//!   `--client-id claude:<tab_id>:<plugin_id>` arg that the host already
//!   passes to every sidecar via the generated MCP config). The
//!   capability handle is NEVER sent over the socket; the bridge looks
//!   up the orchestrator's privileged handle from host state.
//! - The bridge parses `clientId` via `mcp_stdio::ClientId::parse`. The
//!   `plugin_id` segment MUST equal `terminal-mesh` for the cross-tab
//!   read method.
//! - If the calling tab matches the orchestrator's `tab_id`, the bridge
//!   uses the orchestrator's stashed privileged capability and calls
//!   `cross_tab_read_by_tab(...)`.
//! - Otherwise the caller is a regular tab; the bridge only allows
//!   self-read (caller's tab_id == target tab_id) and uses
//!   `read_tab_scrollback_bounded(...)` directly (no capability needed
//!   because the clientId itself is the authorization proof: the host
//!   minted that clientId when it spawned the sidecar).
//!
//! Framing: newline-delimited JSON-RPC 2.0 (same shape as
//! `mcp_stdio::encode_message` / `decode_line`).

use std::path::{Path, PathBuf};

use mcp_stdio::{
    decode_line, encode_message, ClientId, JsonRpcError, JsonRpcId, JsonRpcMessage,
    JsonRpcResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::dispatcher::MountRegistry;
use crate::orchestrator::OrchestratorState;
use crate::terminal_mesh::{
    cross_tab_read_by_tab, read_tab_scrollback_bounded, ListTabsScope, TerminalListTabsEntry,
    TerminalMeshError, TerminalMeshRegistry, MAX_CROSS_TAB_READ_BYTES,
};

/// JSON-RPC error code mapping (private to this module — frontends
/// never see these because the socket is sidecar-only). Mirrors the
/// typed `TerminalMeshError` variants the bridge can surface.
const ERR_PERMISSION_DENIED: i64 = -32001;
const ERR_NOT_FOUND: i64 = -32002;
const ERR_INVALID_CLIENT_ID: i64 = -32003;
const ERR_BOUNDED_READ_FAILED: i64 = -32004;
const ERR_PARSE: i64 = -32700;
const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;

/// Bundled state the bridge needs to authorize and resolve reads.
/// All three fields are internally-Arc-shared, so cloning the bundle
/// (e.g., per accepted connection) is cheap and the bridge observes
/// the same state as the Tauri-managed handles.
#[derive(Clone)]
pub struct HostRpcState {
    pub orchestrator: OrchestratorState,
    pub mount_registry: MountRegistry,
    pub terminal_registry: TerminalMeshRegistry,
    /// Shared with Tauri's app.manage handle (Arc-backed); used by
    /// `handle_list_tabs` to resolve friendly workspace names.
    pub workspaces: crate::workspaces::WorkspaceRegistry,
}

#[derive(Debug, thiserror::Error)]
pub enum HostRpcError {
    #[error("io error setting up socket at `{path}`: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadScrollbackParams {
    client_id: String,
    target_tab_id: String,
    max_bytes: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadScrollbackResult {
    data: String,
    /// Documented hard cap surfaced so the sidecar's tool description
    /// can advertise the truncation contract. NOT a privileged-flag
    /// leak — it's the public bound.
    max_bytes: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTabsParams {
    client_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListTabsResult {
    tabs: Vec<TerminalListTabsEntry>,
}

/// Prepare the socket directory under `${app_data}/host-rpc/`. Returns
/// the absolute socket path. Removes any stale socket file at the path
/// (a previous app instance may have crashed without cleanup). Creates
/// the parent dir with mode 0o700 on Unix.
pub fn prepare_socket_path(app_data: &Path) -> Result<PathBuf, HostRpcError> {
    let dir = app_data.join("host-rpc");
    std::fs::create_dir_all(&dir).map_err(|e| HostRpcError::Io {
        path: dir.clone(),
        message: e.to_string(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let sock = dir.join("host.sock");
    if sock.exists() {
        let _ = std::fs::remove_file(&sock);
    }
    Ok(sock)
}

/// Spawn the bridge listener as a Tokio task. The task lives for the
/// duration of the Tauri runtime; on app shutdown, dropping the task's
/// JoinHandle is sufficient because the socket file is unlinked above
/// at next start.
///
/// Uses `tauri::async_runtime::spawn` rather than bare `tokio::spawn`
/// because this is invoked from the Tauri `setup` hook, which runs
/// synchronously before any tokio-native task has entered the runtime
/// — bare `tokio::spawn` panics there with "no reactor running".
/// `tauri::async_runtime` is backed by tokio and is always available
/// once Tauri's builder has been constructed.
pub fn spawn_bridge(socket_path: PathBuf, state: HostRpcState) {
    tauri::async_runtime::spawn(async move {
        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    socket_path = %socket_path.display(),
                    error = %e,
                    "host_rpc: failed to bind unix socket"
                );
                return;
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &socket_path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
        tracing::info!(
            socket_path = %socket_path.display(),
            "host_rpc: bridge listening"
        );
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, state).await {
                            tracing::warn!(error = %e, "host_rpc: connection ended with error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "host_rpc: accept failed");
                }
            }
        }
    });
}

async fn handle_connection(stream: UnixStream, state: HostRpcState) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // Peer closed.
            return Ok(());
        }
        let response = match decode_line(line.as_bytes()) {
            Ok(JsonRpcMessage::Request(req)) => {
                let result = dispatch_method(&state, &req.method, req.params.unwrap_or(Value::Null));
                match result {
                    Ok(value) => JsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: Some(value),
                        error: None,
                    }),
                    Err(err) => JsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: None,
                        error: Some(err),
                    }),
                }
            }
            Ok(other) => {
                tracing::warn!(?other, "host_rpc: ignoring non-request message");
                continue;
            }
            Err(err) => {
                tracing::warn!(error = %err, line = %line.trim(), "host_rpc: frame parse failed");
                JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: JsonRpcId::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: ERR_PARSE,
                        message: format!("parse error: {err}"),
                        data: None,
                    }),
                })
            }
        };
        let mut bytes = match encode_message(&response) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "host_rpc: encode failed");
                continue;
            }
        };
        bytes.push(b'\n');
        write_half.write_all(&bytes).await?;
        write_half.flush().await?;
    }
}

/// Pure dispatch core, exposed for unit tests so they can drive the
/// method-dispatch logic without standing up a unix socket.
pub fn dispatch_method(
    state: &HostRpcState,
    method: &str,
    params: Value,
) -> Result<Value, JsonRpcError> {
    match method {
        "terminalMesh.readScrollback" => {
            let parsed: ReadScrollbackParams =
                serde_json::from_value(params).map_err(|e| JsonRpcError {
                    code: ERR_INVALID_PARAMS,
                    message: format!("invalid params: {e}"),
                    data: None,
                })?;
            handle_read_scrollback(state, parsed)
        }
        "terminalMesh.listTabs" => {
            let parsed: ListTabsParams =
                serde_json::from_value(params).map_err(|e| JsonRpcError {
                    code: ERR_INVALID_PARAMS,
                    message: format!("invalid params: {e}"),
                    data: None,
                })?;
            handle_list_tabs(state, parsed)
        }
        other => Err(JsonRpcError {
            code: ERR_METHOD_NOT_FOUND,
            message: format!("unknown method `{other}`"),
            data: None,
        }),
    }
}

fn handle_read_scrollback(
    state: &HostRpcState,
    params: ReadScrollbackParams,
) -> Result<Value, JsonRpcError> {
    let caller = ClientId::parse(&params.client_id).map_err(|e| JsonRpcError {
        code: ERR_INVALID_CLIENT_ID,
        message: format!("invalid clientId: {e:?}"),
        data: None,
    })?;
    let (caller_tab_id, plugin_id) = match caller {
        ClientId::Claude { tab_id, plugin_id } => (tab_id.to_string(), plugin_id),
        ClientId::HostUi { .. } => {
            return Err(JsonRpcError {
                code: ERR_INVALID_REQUEST,
                message: "host_ui clientId cannot call terminalMesh.readScrollback".into(),
                data: None,
            });
        }
    };
    if plugin_id != "terminal-mesh" {
        return Err(JsonRpcError {
            code: ERR_INVALID_REQUEST,
            message: format!(
                "terminalMesh.readScrollback requires plugin_id=terminal-mesh; got `{plugin_id}`"
            ),
            data: None,
        });
    }

    let want_bytes = (params.max_bytes as usize).min(MAX_CROSS_TAB_READ_BYTES);

    // If the caller is the orchestrator, use the privileged path.
    // Otherwise, regular self-read is allowed; cross-tab read is denied.
    let orch_snapshot = state.orchestrator.snapshot();
    let is_orchestrator = orch_snapshot
        .as_ref()
        .map(|s| s.tab_id == caller_tab_id)
        .unwrap_or(false);

    let data_result = if is_orchestrator {
        let session = orch_snapshot.expect("snapshot is Some by is_orchestrator branch");
        let handle = session.terminal_mesh_capability.as_deref().ok_or_else(|| {
            JsonRpcError {
                code: ERR_PERMISSION_DENIED,
                message: "orchestrator session has no privileged terminal-mesh capability".into(),
                data: None,
            }
        })?;
        cross_tab_read_by_tab(
            &state.mount_registry,
            &state.terminal_registry,
            handle,
            &params.target_tab_id,
            want_bytes,
        )
    } else if caller_tab_id == params.target_tab_id {
        // Regular tab self-read — clientId proves the caller owns the
        // tab; no capability needed.
        read_tab_scrollback_bounded(&state.terminal_registry, &params.target_tab_id, want_bytes)
    } else {
        return Err(JsonRpcError {
            code: ERR_PERMISSION_DENIED,
            message: format!(
                "regular tab `{caller_tab_id}` may not cross-tab read `{}` (orchestrator-only)",
                params.target_tab_id
            ),
            data: None,
        });
    };

    match data_result {
        Ok(data) => Ok(json!(ReadScrollbackResult {
            data,
            max_bytes: MAX_CROSS_TAB_READ_BYTES as u32,
        })),
        Err(TerminalMeshError::PermissionDenied { message }) => Err(JsonRpcError {
            code: ERR_PERMISSION_DENIED,
            message,
            data: None,
        }),
        Err(TerminalMeshError::NotFound { terminal_id }) => Err(JsonRpcError {
            code: ERR_NOT_FOUND,
            message: format!("target tab/terminal not found: {terminal_id}"),
            data: None,
        }),
        Err(other) => Err(JsonRpcError {
            code: ERR_BOUNDED_READ_FAILED,
            message: other.to_string(),
            data: None,
        }),
    }
}

fn handle_list_tabs(
    state: &HostRpcState,
    params: ListTabsParams,
) -> Result<Value, JsonRpcError> {
    let caller = ClientId::parse(&params.client_id).map_err(|e| JsonRpcError {
        code: ERR_INVALID_CLIENT_ID,
        message: format!("invalid clientId: {e:?}"),
        data: None,
    })?;
    let (caller_tab_id, plugin_id) = match caller {
        ClientId::Claude { tab_id, plugin_id } => (tab_id.to_string(), plugin_id),
        ClientId::HostUi { .. } => {
            return Err(JsonRpcError {
                code: ERR_INVALID_REQUEST,
                message: "host_ui clientId cannot call terminalMesh.listTabs".into(),
                data: None,
            });
        }
    };
    if plugin_id != "terminal-mesh" {
        return Err(JsonRpcError {
            code: ERR_INVALID_REQUEST,
            message: format!(
                "terminalMesh.listTabs requires plugin_id=terminal-mesh; got `{plugin_id}`"
            ),
            data: None,
        });
    }

    // Scope selection: the orchestrator sees every tab; regular
    // workspace callers see ONLY their own tab (sibling-workspace
    // isolation). The is_orchestrator check mirrors
    // `handle_read_scrollback` — both use the recorded orchestrator
    // session's tab id as the identity boundary.
    let is_orchestrator = state
        .orchestrator
        .snapshot()
        .map(|s| s.tab_id == caller_tab_id)
        .unwrap_or(false);
    let scope = if is_orchestrator {
        ListTabsScope::All
    } else {
        ListTabsScope::OwnTab {
            tab_id: caller_tab_id.as_str(),
        }
    };

    let mut tabs = state.terminal_registry.list_tabs(scope);
    // Enrich with friendly workspace names from the persisted
    // workspace registry. Unknown / malformed ids stay `None`; the
    // sidecar tool exposes the `workspaceName` field as optional so
    // MCP clients tolerate missing values.
    for entry in tabs.iter_mut() {
        if let Some(ws_id) = entry.workspace_id.as_deref() {
            entry.workspace_name = state.workspaces.lookup_name(ws_id);
        }
    }
    Ok(json!(ListTabsResult { tabs }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::dispatcher::{insert_orchestrator_mount, MountRegistry};
    use crate::orchestrator::{OrchestratorSession, OrchestratorState};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn bridge_state_with_two_tabs(
        orch_tab_id: &str,
        regular_tab_id: &str,
    ) -> (HostRpcState, String) {
        let mount_registry = MountRegistry::new();
        let terminal_registry = TerminalMeshRegistry::new();

        // Pre-register two terminals with tab ids so lookup_terminal_by_tab works.
        for (tab_id, body) in [
            (orch_tab_id.to_string(), "ORCHESTRATOR_OUTPUT".to_string()),
            (regular_tab_id.to_string(), "REGULAR_TAB_OUTPUT".to_string()),
        ] {
            let id = Uuid::new_v4();
            let (tx, _rx) = mpsc::channel::<terminal_mesh_core::ActorCommand>(1);
            let buf = Arc::new(StdMutex::new(body));
            terminal_registry.record(
                id,
                tx,
                buf,
                Some(tab_id),
                crate::workspace_lifecycle::TabKind::Workspace,
                None,
            );
        }

        // Mint the privileged orchestrator capability.
        let resp = insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            Some(orch_tab_id),
        )
        .expect("terminal-mesh builtin declares cross_tab_read");

        // Record the orchestrator session.
        let orchestrator = OrchestratorState::new();
        orchestrator.record_session(OrchestratorSession {
            terminal_id: Uuid::new_v4(),
            tab_id: orch_tab_id.to_string(),
            mcp_config_path: std::path::PathBuf::from("/tmp/orch.json"),
            terminal_mesh_capability: Some(resp.handle.clone()),
        });

        let state = HostRpcState {
            orchestrator,
            mount_registry,
            terminal_registry,
            workspaces: crate::workspaces::WorkspaceRegistry::empty(
                std::path::PathBuf::from("/tmp/host-rpc-test"),
                None,
            ),
        };
        (state, resp.handle)
    }

    fn make_uuid_tab_id() -> String {
        Uuid::new_v4().to_string()
    }

    #[test]
    fn bridge_orchestrator_client_id_can_read_other_tab() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _handle) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);

        let params = json!({
            "clientId": format!("claude:{orch_tab}:terminal-mesh"),
            "targetTabId": regular_tab,
            "maxBytes": 8192_u32,
        });
        let v = dispatch_method(&state, "terminalMesh.readScrollback", params)
            .expect("orchestrator may read another tab");
        assert_eq!(v["data"], "REGULAR_TAB_OUTPUT");
    }

    #[test]
    fn bridge_orchestrator_client_id_can_read_own_tab() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": format!("claude:{orch_tab}:terminal-mesh"),
            "targetTabId": orch_tab,
            "maxBytes": 8192_u32,
        });
        let v = dispatch_method(&state, "terminalMesh.readScrollback", params)
            .expect("orchestrator may also read its own tab");
        assert_eq!(v["data"], "ORCHESTRATOR_OUTPUT");
    }

    #[test]
    fn bridge_regular_client_id_denies_different_tab() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": format!("claude:{regular_tab}:terminal-mesh"),
            "targetTabId": orch_tab, // different from caller
            "maxBytes": 8192_u32,
        });
        let err = dispatch_method(&state, "terminalMesh.readScrollback", params).unwrap_err();
        assert_eq!(err.code, ERR_PERMISSION_DENIED);
    }

    #[test]
    fn bridge_regular_client_id_can_read_own_tab() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": format!("claude:{regular_tab}:terminal-mesh"),
            "targetTabId": regular_tab,
            "maxBytes": 8192_u32,
        });
        let v = dispatch_method(&state, "terminalMesh.readScrollback", params)
            .expect("regular tab may self-read");
        assert_eq!(v["data"], "REGULAR_TAB_OUTPUT");
    }

    #[test]
    fn bridge_invalid_client_id_returns_invalid_client_id_error() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": "garbage",
            "targetTabId": orch_tab,
            "maxBytes": 8192_u32,
        });
        let err = dispatch_method(&state, "terminalMesh.readScrollback", params).unwrap_err();
        assert_eq!(err.code, ERR_INVALID_CLIENT_ID);
    }

    #[test]
    fn bridge_host_ui_client_id_is_invalid_request() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": "host_ui:terminal-mesh",
            "targetTabId": orch_tab,
            "maxBytes": 8192_u32,
        });
        let err = dispatch_method(&state, "terminalMesh.readScrollback", params).unwrap_err();
        assert_eq!(err.code, ERR_INVALID_REQUEST);
    }

    #[test]
    fn bridge_non_terminal_mesh_plugin_id_is_invalid_request() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": format!("claude:{orch_tab}:gmail"),
            "targetTabId": regular_tab,
            "maxBytes": 8192_u32,
        });
        let err = dispatch_method(&state, "terminalMesh.readScrollback", params).unwrap_err();
        assert_eq!(err.code, ERR_INVALID_REQUEST);
    }

    #[test]
    fn bridge_unknown_target_tab_returns_not_found() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let unknown_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let params = json!({
            "clientId": format!("claude:{orch_tab}:terminal-mesh"),
            "targetTabId": unknown_tab,
            "maxBytes": 8192_u32,
        });
        let err = dispatch_method(&state, "terminalMesh.readScrollback", params).unwrap_err();
        assert_eq!(err.code, ERR_NOT_FOUND);
    }

    #[test]
    fn bridge_unknown_method_returns_method_not_found() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);
        let err = dispatch_method(&state, "no.such.method", json!({})).unwrap_err();
        assert_eq!(err.code, ERR_METHOD_NOT_FOUND);
    }

    // ----- Round 39: end-to-end app-spawn path validation -----
    //
    // The Round 38 tests pre-registered tab ids directly. These
    // regressions exercise the path the actual frontend takes:
    // call `terminal_mesh::spawn_into_registry`-equivalent helpers
    // with a tab_id, then prove the bridge resolves it.

    #[test]
    fn bridge_resolves_tab_id_registered_via_record_helper() {
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);

        // Spawn a NEW workspace tab using the same `record()` call
        // path that `spawn_into_registry` walks when the frontend
        // sets `req.tab_id`. The bridge must resolve `target_tab_id`
        // → terminal_id via the tab_index.
        let extra_tab_id = make_uuid_tab_id();
        let extra_terminal_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<terminal_mesh_core::ActorCommand>(1);
        let buf = Arc::new(StdMutex::new("WORKSPACE_TAB_OUTPUT".into()));
        state
            .terminal_registry
            .record(
                extra_terminal_id,
                tx,
                buf,
                Some(extra_tab_id.clone()),
                crate::workspace_lifecycle::TabKind::Workspace,
                None,
            );

        // Orchestrator reads the new workspace tab end-to-end.
        let params = json!({
            "clientId": format!("claude:{orch_tab}:terminal-mesh"),
            "targetTabId": extra_tab_id,
            "maxBytes": 8192_u32,
        });
        let v = dispatch_method(&state, "terminalMesh.readScrollback", params)
            .expect("orchestrator may read the freshly spawned workspace tab");
        assert_eq!(v["data"], "WORKSPACE_TAB_OUTPUT");
    }

    #[test]
    fn bridge_returns_not_found_when_terminal_registered_without_tab_id() {
        // Negative variant: if a terminal is spawned WITHOUT a tab_id
        // (the back-compat path), bridge lookup by tab_id MUST return
        // NotFound — proving the index path is the authoritative
        // mechanism, not just a heuristic.
        let orch_tab = make_uuid_tab_id();
        let regular_tab = make_uuid_tab_id();
        let (state, _) = bridge_state_with_two_tabs(&orch_tab, &regular_tab);

        let no_tab_terminal_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<terminal_mesh_core::ActorCommand>(1);
        let buf = Arc::new(StdMutex::new("UNINDEXED".into()));
        state
            .terminal_registry
            .record(
                no_tab_terminal_id,
                tx,
                buf,
                None,
                crate::workspace_lifecycle::TabKind::Workspace,
                None,
            );

        // The orchestrator tries to address it by the OLD (terminal_id)
        // value as a "tab_id". Since the tab_index is empty for it,
        // resolution fails with NotFound — exactly the Codex round-38
        // contract: regular workspace terminals must be registered
        // under their tab id or they're not addressable.
        let params = json!({
            "clientId": format!("claude:{orch_tab}:terminal-mesh"),
            "targetTabId": no_tab_terminal_id.to_string(),
            "maxBytes": 8192_u32,
        });
        let err = dispatch_method(&state, "terminalMesh.readScrollback", params).unwrap_err();
        assert_eq!(err.code, ERR_NOT_FOUND);
    }

    /// Build a state where the orchestrator slot is recorded as
    /// `TabKind::Orchestrator` AND two regular workspace tabs are
    /// recorded as `TabKind::Workspace`. The auth filter in
    /// `handle_list_tabs` looks at the orchestrator session's tab id
    /// to decide whether to include the orchestrator entry, but the
    /// snapshot store needs the orchestrator tab to be marked
    /// `Orchestrator` so the filter actually has something to skip.
    /// Returns the state plus the two workspace tab ids so tests can
    /// construct valid `claude:<uuid>:terminal-mesh` clientIds.
    fn bridge_state_for_list_tabs(orch_tab_id: &str) -> (HostRpcState, String, String) {
        let mount_registry = MountRegistry::new();
        let terminal_registry = TerminalMeshRegistry::new();

        // Orchestrator slot — kind=Orchestrator, no workspace id.
        {
            let id = Uuid::new_v4();
            let (tx, _rx) = mpsc::channel::<terminal_mesh_core::ActorCommand>(1);
            let buf = Arc::new(StdMutex::new(String::new()));
            terminal_registry.record(
                id,
                tx,
                buf,
                Some(orch_tab_id.to_string()),
                crate::workspace_lifecycle::TabKind::Orchestrator,
                None,
            );
        }
        // Two workspace tabs — clientId parsing requires the tab id
        // to be a valid UUID, so generate fresh ones here.
        let ws_tab_a = make_uuid_tab_id();
        let ws_tab_b = make_uuid_tab_id();
        for (tab_id, ws_id) in [
            (ws_tab_a.clone(), "workspace-A"),
            (ws_tab_b.clone(), "workspace-B"),
        ] {
            let id = Uuid::new_v4();
            let (tx, _rx) = mpsc::channel::<terminal_mesh_core::ActorCommand>(1);
            let buf = Arc::new(StdMutex::new(String::new()));
            terminal_registry.record(
                id,
                tx,
                buf,
                Some(tab_id),
                crate::workspace_lifecycle::TabKind::Workspace,
                Some(ws_id.into()),
            );
        }

        let orchestrator = OrchestratorState::new();
        orchestrator.record_session(OrchestratorSession {
            terminal_id: Uuid::new_v4(),
            tab_id: orch_tab_id.to_string(),
            mcp_config_path: std::path::PathBuf::from("/tmp/orch.json"),
            terminal_mesh_capability: None,
        });

        let state = HostRpcState {
            orchestrator,
            mount_registry,
            terminal_registry,
            workspaces: crate::workspaces::WorkspaceRegistry::empty(
                std::path::PathBuf::from("/tmp/host-rpc-test"),
                None,
            ),
        };
        (state, ws_tab_a, ws_tab_b)
    }

    #[test]
    fn host_rpc_list_tabs_rejects_unknown_client_id() {
        let orch_tab = make_uuid_tab_id();
        let (state, _, _) = bridge_state_for_list_tabs(&orch_tab);
        let params = json!({ "clientId": "this is not a valid client id" });
        let err = dispatch_method(&state, "terminalMesh.listTabs", params).unwrap_err();
        assert_eq!(err.code, ERR_INVALID_CLIENT_ID);
    }

    #[test]
    fn host_rpc_list_tabs_rejects_host_ui_client_id() {
        let orch_tab = make_uuid_tab_id();
        let (state, _, _) = bridge_state_for_list_tabs(&orch_tab);
        let params = json!({ "clientId": "host_ui:terminal-mesh" });
        let err = dispatch_method(&state, "terminalMesh.listTabs", params).unwrap_err();
        assert_eq!(err.code, ERR_INVALID_REQUEST);
    }

    #[test]
    fn host_rpc_list_tabs_orchestrator_client_id_includes_orchestrator() {
        let orch_tab = make_uuid_tab_id();
        let (state, _, _) = bridge_state_for_list_tabs(&orch_tab);
        let params = json!({
            "clientId": format!("claude:{orch_tab}:terminal-mesh"),
        });
        let v = dispatch_method(&state, "terminalMesh.listTabs", params)
            .expect("orchestrator client may list every tab");
        let tabs = v["tabs"].as_array().expect("tabs is an array");
        assert_eq!(tabs.len(), 3, "orchestrator + 2 workspace tabs");
        let kinds: Vec<&str> = tabs
            .iter()
            .filter_map(|t| t["tabKind"].as_str())
            .collect();
        assert!(kinds.contains(&"Orchestrator"));
    }

    #[test]
    fn host_rpc_list_tabs_populates_workspace_name_from_registry() {
        // Seed the workspace registry with a known name + record a
        // tab whose workspace_id matches; assert the bridge response
        // carries the friendly name in workspaceName.
        use crate::workspaces::WorkspaceRecord;
        let orch_tab = make_uuid_tab_id();
        let (mut state, _ws_tab_a, _ws_tab_b) = bridge_state_for_list_tabs(&orch_tab);

        let real_ws_id = Uuid::new_v4();
        state.workspaces.insert_record_for_tests(WorkspaceRecord {
            workspace_id: real_ws_id,
            name: "Alice's Project".into(),
            path: std::path::PathBuf::from("/tmp/alice"),
            created_at: "2025-01-01T00:00:00Z".into(),
            last_used_at: "2025-01-01T00:00:00Z".into(),
            open_tab_id: None,
            conversation_rounds_count: 0,
        });

        // Record a tab bound to that workspace id.
        let id = Uuid::new_v4();
        let tab_id = make_uuid_tab_id();
        let (tx, _rx) = mpsc::channel::<terminal_mesh_core::ActorCommand>(1);
        let buf = Arc::new(StdMutex::new(String::new()));
        state.terminal_registry.record(
            id,
            tx,
            buf,
            Some(tab_id.clone()),
            crate::workspace_lifecycle::TabKind::Workspace,
            Some(real_ws_id.to_string()),
        );

        let params = json!({
            "clientId": format!("claude:{tab_id}:terminal-mesh"),
        });
        let v = dispatch_method(&state, "terminalMesh.listTabs", params)
            .expect("regular workspace client may list its own tab");
        let tabs = v["tabs"].as_array().expect("tabs is an array");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0]["workspaceName"], "Alice's Project");
        assert_eq!(tabs[0]["workspaceId"], real_ws_id.to_string());
    }

    #[test]
    fn host_rpc_list_tabs_workspace_client_id_returns_only_own_tab() {
        let orch_tab = make_uuid_tab_id();
        let (state, ws_tab_a, _ws_tab_b) = bridge_state_for_list_tabs(&orch_tab);
        // Regular workspace client must see ONLY its own tab. Sibling
        // workspace tabs and the orchestrator slot are both filtered.
        let params = json!({
            "clientId": format!("claude:{ws_tab_a}:terminal-mesh"),
        });
        let v = dispatch_method(&state, "terminalMesh.listTabs", params)
            .expect("regular workspace client may list its own tab");
        let tabs = v["tabs"].as_array().expect("tabs is an array");
        assert_eq!(
            tabs.len(),
            1,
            "regular workspace client must see ONLY its own tab (no siblings, no orchestrator)"
        );
        assert_eq!(tabs[0]["tabId"], ws_tab_a);
        assert_eq!(tabs[0]["workspaceId"], "workspace-A");
        assert_eq!(tabs[0]["tabKind"], "Workspace");
        // Sibling workspace-B must NOT appear.
        for tab in tabs {
            assert_ne!(tab["workspaceId"], "workspace-B");
        }
    }
}
