//! Host-side wiring for Terminal Mesh tabs (AC-4.1 + AC-4.2 frontend).
//!
//! Owns a registry of live `TerminalActor`s and exposes 5 Tauri
//! commands the React/xterm.js layer uses to spawn / write stdin /
//! resize / shutdown / read scrollback. Per-spawn an event-forwarder
//! task bridges `TerminalEventEnvelope`s + `BufferTruncated` status
//! events from the actor's mpsc onto Tauri webview events keyed by
//! `terminal://{terminal_id}/event` and `terminal://{terminal_id}/status`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use terminal_mesh_core::{
    ActorCommand, ActorError, BufferTruncated, TerminalActor, TerminalEvent,
    TerminalEventEnvelope, TerminalHandle, TerminalSpec,
};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Maximum scrollback retained in-memory per terminal so the frontend
/// can ask `terminal_scrollback` to restore content on tab remount.
/// Bounded to keep the registry from drifting unbounded over the
/// lifetime of long-running terminals.
const SCROLLBACK_RETENTION_BYTES: usize = 64 * 1024;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

#[derive(Debug, thiserror::Error)]
pub enum TerminalMeshError {
    #[error("terminal `{terminal_id}` not found")]
    NotFound { terminal_id: String },

    #[error("invalid terminal_id `{raw}`: {message}")]
    InvalidTerminalId { raw: String, message: String },

    #[error("spawn failed: {message}")]
    Spawn { message: String },

    #[error("io error in `{context}`: {message}")]
    Io { context: String, message: String },

    /// task21 / AC-3.3: the calling capability is valid but lacks the
    /// privilege required for the requested action. Today only the cross-tab
    /// scrollback read uses this; the calling capability's MountEntry
    /// `cross_tab_read_flag` is `false`. The DTO surface intentionally
    /// names this `permissionDenied` (not `crossTabReadDenied`) so the wire
    /// shape does not leak the privileged-flag terminology.
    #[error("permission denied: {message}")]
    PermissionDenied { message: String },
}

impl From<ActorError> for TerminalMeshError {
    fn from(err: ActorError) -> Self {
        match err {
            ActorError::Pty(m) => TerminalMeshError::Spawn { message: m },
            ActorError::Io(m) => TerminalMeshError::Io {
                context: "actor".into(),
                message: m,
            },
            ActorError::AlreadyShutdown => TerminalMeshError::NotFound {
                terminal_id: "<unknown>".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum TerminalMeshErrorDto {
    NotFound { terminal_id: String },
    InvalidTerminalId { raw: String, message: String },
    Spawn { message: String },
    Io { context: String, message: String },
    PermissionDenied { message: String },
}

impl From<&TerminalMeshError> for TerminalMeshErrorDto {
    fn from(err: &TerminalMeshError) -> Self {
        match err {
            TerminalMeshError::NotFound { terminal_id } => Self::NotFound {
                terminal_id: terminal_id.clone(),
            },
            TerminalMeshError::InvalidTerminalId { raw, message } => Self::InvalidTerminalId {
                raw: raw.clone(),
                message: message.clone(),
            },
            TerminalMeshError::Spawn { message } => Self::Spawn {
                message: message.clone(),
            },
            TerminalMeshError::Io { context, message } => Self::Io {
                context: context.clone(),
                message: message.clone(),
            },
            TerminalMeshError::PermissionDenied { message } => Self::PermissionDenied {
                message: message.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSpawnRequest {
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    /// task21 / AC-3.3: optional workspace tab id. When set, the
    /// registry indexes the spawned terminal under this tab id so
    /// the host RPC bridge can resolve `target_tab_id → terminal_id`.
    #[serde(default)]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSpawnResponse {
    pub terminal_id: String,
}

struct TerminalSession {
    command_tx: mpsc::Sender<ActorCommand>,
    scrollback: Arc<StdMutex<String>>,
    /// task21 / AC-3.3: the tab id this session belongs to (the
    /// frontend's workspace tab id for regular tabs; the orchestrator's
    /// `OrchestratorSession::tab_id` for the orchestrator). Used by
    /// the host RPC bridge to resolve `target_tab_id → terminal_id`.
    tab_id: Option<String>,
}

/// Tauri-managed registry of live terminal sessions.
///
/// task21 / AC-3.3: shared via internal `Arc<Mutex<...>>` so the
/// Tauri-managed handle and the host RPC bridge clone can both hold
/// `TerminalMeshRegistry` by value while sharing the same underlying
/// state.
#[derive(Clone)]
pub struct TerminalMeshRegistry {
    inner: Arc<StdMutex<HashMap<Uuid, TerminalSession>>>,
    /// Secondary index for the host RPC bridge so an authorized
    /// `target_tab_id` resolves to its current `terminal_id` in O(1).
    /// Maintained in lockstep with `inner` by `record` and `forget`.
    tab_index: Arc<StdMutex<HashMap<String, Uuid>>>,
}

impl Default for TerminalMeshRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalMeshRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(HashMap::new())),
            tab_index: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(crate) fn record(
        &self,
        id: Uuid,
        command_tx: mpsc::Sender<ActorCommand>,
        scrollback: Arc<StdMutex<String>>,
        tab_id: Option<String>,
    ) {
        let mut guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        if let Some(t) = tab_id.as_deref() {
            let mut idx = self.tab_index.lock().expect("TerminalMeshRegistry tab_index poisoned");
            idx.insert(t.to_string(), id);
        }
        guard.insert(
            id,
            TerminalSession {
                command_tx,
                scrollback,
                tab_id,
            },
        );
    }

    pub fn lookup_command_tx(&self, id: Uuid) -> Option<mpsc::Sender<ActorCommand>> {
        let guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.get(&id).map(|s| s.command_tx.clone())
    }

    pub(crate) fn lookup_scrollback(&self, id: Uuid) -> Option<Arc<StdMutex<String>>> {
        let guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.get(&id).map(|s| s.scrollback.clone())
    }

    /// task21 / AC-3.3: tab-id → terminal_id resolution for the host
    /// RPC bridge's `terminalMesh.readScrollback` request. Returns
    /// `None` if no live session is registered for `tab_id`.
    pub fn lookup_terminal_by_tab(&self, tab_id: &str) -> Option<Uuid> {
        let idx = self.tab_index.lock().expect("TerminalMeshRegistry tab_index poisoned");
        idx.get(tab_id).copied()
    }

    pub fn forget(&self, id: Uuid) {
        let mut guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        if let Some(session) = guard.remove(&id) {
            if let Some(t) = session.tab_id.as_deref() {
                let mut idx = self.tab_index.lock().expect("TerminalMeshRegistry tab_index poisoned");
                // Only remove if the index still points to this id
                // (a re-record under the same tab_id should win).
                if idx.get(t).copied() == Some(id) {
                    idx.remove(t);
                }
            }
        }
    }

    /// True if `id` is currently registered (i.e., the actor has not
    /// naturally exited and `forget` has not been called yet). Used
    /// by `orchestrator_status` to detect a stale recorded session.
    pub fn contains(&self, id: Uuid) -> bool {
        let guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.contains_key(&id)
    }

    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        let guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.len()
    }
}

fn parse_terminal_id(raw: &str) -> Result<Uuid, TerminalMeshError> {
    Uuid::parse_str(raw).map_err(|e| TerminalMeshError::InvalidTerminalId {
        raw: raw.to_string(),
        message: e.to_string(),
    })
}

fn topic_event(id: Uuid) -> String {
    format!("terminal://{id}/event")
}

fn topic_status(id: Uuid) -> String {
    format!("terminal://{id}/status")
}

fn resolve_default_shell(env: &[(String, String)]) -> PathBuf {
    if let Some((_, v)) = env.iter().find(|(k, _)| k == "SHELL") {
        return PathBuf::from(v);
    }
    if let Ok(s) = std::env::var("SHELL") {
        return PathBuf::from(s);
    }
    PathBuf::from("/bin/sh")
}

fn resolve_default_cwd() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Append `chunk` to the scrollback ring, dropping the oldest bytes to
/// keep the buffer under [`SCROLLBACK_RETENTION_BYTES`].
fn append_scrollback(buf: &Arc<StdMutex<String>>, chunk: &[u8]) {
    let mut guard = buf.lock().expect("scrollback mutex poisoned");
    // Append as lossy UTF-8 — the bytes the actor surfaces are already
    // post-ring-buffer (the actor's ring guarantees UTF-8 safety for
    // scrollback reads), but here we accept arbitrary Output bytes
    // that may be in the middle of a codepoint across chunks. Lossy
    // conversion is acceptable for the display-only restore path.
    let s = String::from_utf8_lossy(chunk);
    guard.push_str(&s);
    if guard.len() > SCROLLBACK_RETENTION_BYTES {
        // Truncate from the front. Naive byte-level truncation could
        // split a UTF-8 codepoint; round to the next char boundary.
        let drop_bytes = guard.len() - SCROLLBACK_RETENTION_BYTES;
        let mut idx = drop_bytes;
        while !guard.is_char_boundary(idx) {
            idx += 1;
            if idx >= guard.len() {
                guard.clear();
                return;
            }
        }
        *guard = guard.split_off(idx);
    }
}

#[tauri::command]
pub async fn terminal_spawn(
    req: TerminalSpawnRequest,
    app: AppHandle,
    registry: State<'_, TerminalMeshRegistry>,
) -> Result<TerminalSpawnResponse, TerminalMeshErrorDto> {
    let cols = if req.cols == 0 { DEFAULT_COLS } else { req.cols };
    let rows = if req.rows == 0 { DEFAULT_ROWS } else { req.rows };
    let cwd = req
        .cwd
        .map(PathBuf::from)
        .or_else(resolve_default_cwd)
        .filter(|p| p.exists());
    let command = resolve_default_shell(&req.env);

    let spec = TerminalSpec {
        terminal_id: Uuid::new_v4(),
        command,
        args: vec![],
        cwd,
        env: req.env,
        cols,
        rows,
    };

    let terminal_id = spawn_into_registry(spec, &app, &registry, req.tab_id)
        .map_err(|e| TerminalMeshErrorDto::from(&e))?;
    Ok(TerminalSpawnResponse {
        terminal_id: terminal_id.to_string(),
    })
}

/// Spawn a `TerminalActor` for `spec`, register it under its
/// `terminal_id` in `registry`, and start the per-terminal event +
/// status forwarder tasks so the frontend can subscribe via the
/// standard `terminal://{id}/event` and `/status` topics. Returns
/// the registered terminal_id on success.
///
/// Factored out so the orchestrator (and any future host-side spawn
/// path that needs a custom command/args) can register with the same
/// registry + forwarder semantics as `terminal_spawn` without
/// duplicating wiring.
pub(crate) fn spawn_into_registry(
    spec: TerminalSpec,
    app: &AppHandle,
    registry: &TerminalMeshRegistry,
    tab_id: Option<String>,
) -> Result<Uuid, TerminalMeshError> {
    let handle = TerminalActor::spawn(spec).map_err(TerminalMeshError::from)?;
    let TerminalHandle {
        terminal_id: id,
        events_rx,
        command_tx,
        status_rx,
    } = handle;
    let scrollback = Arc::new(StdMutex::new(String::new()));
    registry.record(id, command_tx, scrollback.clone(), tab_id);

    let app_for_events = app.clone();
    let scrollback_for_events = scrollback.clone();
    tokio::spawn(forward_events_to_webview(
        id,
        events_rx,
        app_for_events,
        scrollback_for_events,
    ));
    let app_for_status = app.clone();
    tokio::spawn(forward_status_to_webview(id, status_rx, app_for_status));

    Ok(id)
}

async fn forward_events_to_webview(
    id: Uuid,
    mut rx: mpsc::Receiver<TerminalEventEnvelope>,
    app: AppHandle,
    scrollback: Arc<StdMutex<String>>,
) {
    let topic = topic_event(id);
    while let Some(env) = rx.recv().await {
        if let TerminalEvent::Output { bytes } = &env.event {
            append_scrollback(&scrollback, bytes);
        }
        // Round 40 (task22): NeedsAttention events are also routed
        // through the host-level NotificationService for dedup, the
        // tray-entry ring, and native macOS notifications. The
        // service is best-effort; if the host hasn't managed it
        // (test paths, bootstrap failure) skip silently.
        if matches!(env.event, TerminalEvent::NeedsAttention { .. }) {
            if let Some(service) = app.try_state::<crate::notification::NotificationService>() {
                let _decision = service.on_needs_attention(&env);
                // Emit a frontend event so the tray window can
                // refetch its entries list. Best-effort.
                if let Err(err) = app.emit("tray://updated", &serde_json::json!({})) {
                    tracing::warn!(%err, "tray://updated emit failed");
                }
            }
        }
        if let Err(err) = app.emit(&topic, &env) {
            tracing::warn!(%id, %err, "terminal event emit failed");
        }
    }
    // The actor's event channel closed: the PTY exited naturally (or
    // crashed) and no more events will arrive. Evict the registry
    // entry so liveness checks (e.g. `orchestrator_status`) observe
    // the missing terminal and clear stale recorded sessions.
    // Idempotent with the explicit `terminal_shutdown` path.
    if let Some(registry) = app.try_state::<TerminalMeshRegistry>() {
        registry.forget(id);
    }
}

async fn forward_status_to_webview(
    id: Uuid,
    mut rx: mpsc::Receiver<BufferTruncated>,
    app: AppHandle,
) {
    let topic = topic_status(id);
    while let Some(status) = rx.recv().await {
        if let Err(err) = app.emit(&topic, &status) {
            tracing::warn!(%id, %err, "terminal status emit failed");
        }
    }
}

#[tauri::command]
pub async fn terminal_write_stdin(
    terminal_id: String,
    data: String,
    registry: State<'_, TerminalMeshRegistry>,
) -> Result<(), TerminalMeshErrorDto> {
    let id = parse_terminal_id(&terminal_id).map_err(|e| TerminalMeshErrorDto::from(&e))?;
    let tx = registry
        .lookup_command_tx(id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: id.to_string(),
        })
        .map_err(|e| TerminalMeshErrorDto::from(&e))?;
    tx.send(ActorCommand::WriteStdin(data.into_bytes()))
        .await
        .map_err(|e| {
            TerminalMeshErrorDto::from(&TerminalMeshError::Io {
                context: "command_tx send".into(),
                message: e.to_string(),
            })
        })?;
    Ok(())
}

#[tauri::command]
pub async fn terminal_resize(
    terminal_id: String,
    cols: u16,
    rows: u16,
    registry: State<'_, TerminalMeshRegistry>,
) -> Result<(), TerminalMeshErrorDto> {
    let id = parse_terminal_id(&terminal_id).map_err(|e| TerminalMeshErrorDto::from(&e))?;
    let tx = registry
        .lookup_command_tx(id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: id.to_string(),
        })
        .map_err(|e| TerminalMeshErrorDto::from(&e))?;
    tx.send(ActorCommand::Resize { cols, rows })
        .await
        .map_err(|e| {
            TerminalMeshErrorDto::from(&TerminalMeshError::Io {
                context: "command_tx send".into(),
                message: e.to_string(),
            })
        })?;
    Ok(())
}

#[tauri::command]
pub async fn terminal_shutdown(
    terminal_id: String,
    registry: State<'_, TerminalMeshRegistry>,
) -> Result<(), TerminalMeshErrorDto> {
    let id = parse_terminal_id(&terminal_id).map_err(|e| TerminalMeshErrorDto::from(&e))?;
    let tx = registry
        .lookup_command_tx(id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: id.to_string(),
        })
        .map_err(|e| TerminalMeshErrorDto::from(&e))?;
    // Best-effort send; if the actor has already exited the channel
    // is closed and that's acceptable.
    let _ = tx.send(ActorCommand::Shutdown).await;
    registry.forget(id);
    Ok(())
}

#[tauri::command]
pub fn terminal_scrollback(
    terminal_id: String,
    max_bytes: u32,
    registry: State<'_, TerminalMeshRegistry>,
) -> Result<String, TerminalMeshErrorDto> {
    let id = parse_terminal_id(&terminal_id).map_err(|e| TerminalMeshErrorDto::from(&e))?;
    let scrollback = registry
        .lookup_scrollback(id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: id.to_string(),
        })
        .map_err(|e| TerminalMeshErrorDto::from(&e))?;
    let guard = scrollback.lock().expect("scrollback mutex poisoned");
    let want = max_bytes as usize;
    if guard.len() <= want {
        return Ok(guard.clone());
    }
    // Trim from the front to keep the most recent N bytes; round
    // forward to the next UTF-8 char boundary.
    let drop_bytes = guard.len() - want;
    let mut idx = drop_bytes;
    while !guard.is_char_boundary(idx) {
        idx += 1;
        if idx >= guard.len() {
            return Ok(String::new());
        }
    }
    Ok(guard[idx..].to_string())
}

/// task21 / AC-3.3: hard cap on a single cross-tab scrollback read,
/// independent of the caller-requested `max_bytes`. Documented as the
/// bounded-read max-bytes contract surfaced to the orchestrator's
/// terminal-mesh MCP sidecar; the dispatcher's `cross_tab_read_flag`
/// authorization is the gate, this constant is the size cap. 256 KB
/// chosen as 4× the per-PTY scrollback retention so a single read can
/// always cover at least one full buffer plus headroom.
pub const MAX_CROSS_TAB_READ_BYTES: usize = 256 * 1024;

/// task21 / AC-3.3: pure inner helper used by both the Tauri command
/// shim and unit tests. Separated so tests can drive the auth path
/// without standing up a Tauri app. Returns the most-recent slice of
/// the target's scrollback bounded by `min(max_bytes,
/// MAX_CROSS_TAB_READ_BYTES)`, UTF-8 char-boundary safe at the start.
///
/// Authorization (per AC-3.3 negative test): a `MountEntry` with
/// `cross_tab_read_flag = false` ALWAYS returns
/// `TerminalMeshError::PermissionDenied`, regardless of the target id.
/// The DTO `kind` is `permissionDenied` so the privileged-flag term
/// does not leak into wire shapes.
pub fn cross_tab_read_inner(
    mount_registry: &crate::dispatcher::MountRegistry,
    terminal_registry: &TerminalMeshRegistry,
    capability_handle: &str,
    target_terminal_id: &str,
    max_bytes: usize,
) -> Result<String, TerminalMeshError> {
    let entry = authorize_for_read_scrollback(mount_registry, capability_handle)?;

    let id = parse_terminal_id(target_terminal_id)?;
    let scrollback = terminal_registry
        .lookup_scrollback(id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: id.to_string(),
        })?;

    // Silence unused-variable lint; the entry is only used for its
    // side effect of being validated. Future per-tab ACLs may key on
    // entry.tab_id here.
    let _ = entry;
    Ok(bound_scrollback_tail(&scrollback, max_bytes))
}

/// task21 / AC-3.3 Round 39: shared authorization helper for the
/// `terminal_mesh.read_scrollback` privileged read endpoint. Returns
/// the validated `MountEntry` on success. Three gates, in order:
///
/// 1. Capability handle validates via
///    `dispatcher::authorize_capability_handle("terminal-mesh")`.
/// 2. `entry.cross_tab_read_flag` is `true` (Rust-only privileged
///    grant minted by `dispatcher::insert_orchestrator_mount`).
/// 3. `entry.permissions` contains EVERY permission listed by
///    `dispatcher::command_required_permissions("terminal-mesh",
///    "read_scrollback")` — the built-in command metadata. If the
///    metadata is missing (e.g., command renamed without updating
///    the registry), the read is denied with a clear message.
///
/// This replaces the Round-38 ad-hoc `entry.permissions.contains(
/// "cross_tab_read")` check with a metadata-driven loop, so changing
/// the command's declared permissions in
/// `builtin_plugins::BUILTIN_PLUGINS` changes the enforcement at the
/// read site without further code edits.
fn authorize_for_read_scrollback(
    mount_registry: &crate::dispatcher::MountRegistry,
    capability_handle: &str,
) -> Result<crate::dispatcher::MountEntry, TerminalMeshError> {
    let entry = crate::dispatcher::authorize_capability_handle(
        mount_registry,
        capability_handle,
        "terminal-mesh",
    )
    .map_err(|dto| TerminalMeshError::PermissionDenied {
        message: format!("capability rejected: {dto}"),
    })?;

    if !entry.cross_tab_read_flag {
        return Err(TerminalMeshError::PermissionDenied {
            message: "capability lacks cross-tab read privilege".into(),
        });
    }

    check_command_permissions(&entry, "terminal-mesh", "read_scrollback")?;
    Ok(entry)
}

/// Validate that `entry.permissions` covers every declared permission
/// for `(plugin_id, command_name)` per the dispatcher's metadata
/// (manifest `PLUGINS` or fallthrough `BUILTIN_PLUGINS`). Returns
/// `PermissionDenied` if the metadata is missing OR if any required
/// permission is absent from the entry. Pure helper — public so the
/// host RPC bridge can reuse it for future endpoints.
pub fn check_command_permissions(
    entry: &crate::dispatcher::MountEntry,
    plugin_id: &str,
    command_name: &str,
) -> Result<(), TerminalMeshError> {
    let required = crate::dispatcher::command_required_permissions(plugin_id, command_name)
        .ok_or_else(|| TerminalMeshError::PermissionDenied {
            message: format!(
                "command metadata missing for `{plugin_id}.{command_name}`; refusing to authorize"
            ),
        })?;
    for perm in &required {
        if !entry.permissions.iter().any(|p| p == perm) {
            return Err(TerminalMeshError::PermissionDenied {
                message: format!(
                    "capability does not declare `{perm}` permission required by `{plugin_id}.{command_name}`"
                ),
            });
        }
    }
    Ok(())
}

/// task21 / AC-3.3 — same as `cross_tab_read_inner` but keyed by
/// `target_tab_id` (matching AC text `tab_id=X`). Resolves to the
/// current terminal id via the `TerminalMeshRegistry` tab index then
/// reads the bounded scrollback tail. Used by `host_rpc` and the
/// Tauri command shim for the orchestrator's MCP tool path.
pub fn cross_tab_read_by_tab(
    mount_registry: &crate::dispatcher::MountRegistry,
    terminal_registry: &TerminalMeshRegistry,
    capability_handle: &str,
    target_tab_id: &str,
    max_bytes: usize,
) -> Result<String, TerminalMeshError> {
    let _entry = authorize_for_read_scrollback(mount_registry, capability_handle)?;
    let terminal_id = terminal_registry
        .lookup_terminal_by_tab(target_tab_id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: target_tab_id.to_string(),
        })?;
    let scrollback = terminal_registry
        .lookup_scrollback(terminal_id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: terminal_id.to_string(),
        })?;
    Ok(bound_scrollback_tail(&scrollback, max_bytes))
}

/// task21 / AC-3.3 unauthenticated self-read used by the host RPC
/// bridge when the calling clientId proves the caller owns the tab
/// (e.g., `caller_tab_id == target_tab_id` for a regular tab — no
/// capability handle needed because the clientId itself is the
/// authorization proof). Bounded the same way as the privileged path.
pub fn read_tab_scrollback_bounded(
    terminal_registry: &TerminalMeshRegistry,
    target_tab_id: &str,
    max_bytes: usize,
) -> Result<String, TerminalMeshError> {
    let terminal_id = terminal_registry
        .lookup_terminal_by_tab(target_tab_id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: target_tab_id.to_string(),
        })?;
    let scrollback = terminal_registry
        .lookup_scrollback(terminal_id)
        .ok_or_else(|| TerminalMeshError::NotFound {
            terminal_id: terminal_id.to_string(),
        })?;
    Ok(bound_scrollback_tail(&scrollback, max_bytes))
}

/// Shared tail-bounded read helper. Trims from the front to keep the
/// most-recent `min(max_bytes, MAX_CROSS_TAB_READ_BYTES)` bytes; rounds
/// the start index forward to the next UTF-8 char boundary.
fn bound_scrollback_tail(
    scrollback: &Arc<StdMutex<String>>,
    max_bytes: usize,
) -> String {
    let want = max_bytes.min(MAX_CROSS_TAB_READ_BYTES);
    let guard = scrollback.lock().expect("scrollback mutex poisoned");
    if guard.len() <= want {
        return guard.clone();
    }
    let drop_bytes = guard.len() - want;
    let mut idx = drop_bytes;
    while !guard.is_char_boundary(idx) {
        idx += 1;
        if idx >= guard.len() {
            return String::new();
        }
    }
    guard[idx..].to_string()
}

/// task21 / AC-3.3 Tauri command: orchestrator-side cross-tab scrollback
/// read. Routes through [`cross_tab_read_inner`]; failure DTOs are the
/// standard `TerminalMeshErrorDto` (kind `permissionDenied` for denied
/// reads, `notFound` for unknown targets, `invalidTerminalId` for
/// malformed UUIDs). The capability handle is the orchestrator's
/// `terminal-mesh` mount handle minted by
/// [`crate::dispatcher::insert_orchestrator_mount`].
#[tauri::command]
pub fn terminal_mesh_cross_tab_read_scrollback(
    capability: String,
    target_terminal_id: String,
    max_bytes: u32,
    registry: State<'_, TerminalMeshRegistry>,
    mount_registry: State<'_, crate::dispatcher::MountRegistry>,
) -> Result<String, TerminalMeshErrorDto> {
    cross_tab_read_inner(
        &mount_registry,
        &registry,
        &capability,
        &target_terminal_id,
        max_bytes as usize,
    )
    .map_err(|e| TerminalMeshErrorDto::from(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;

    #[test]
    fn registry_round_trip() {
        let r = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let buf = StdArc::new(StdMutex::new(String::new()));
        r.record(id, tx.clone(), buf.clone(), None);
        assert!(r.lookup_command_tx(id).is_some());
        assert!(r.lookup_scrollback(id).is_some());
        assert_eq!(r.active_count(), 1);
        r.forget(id);
        assert!(r.lookup_command_tx(id).is_none());
        assert_eq!(r.active_count(), 0);
    }

    #[test]
    fn terminal_registry_contains_returns_false_after_forget() {
        let r = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let buf = StdArc::new(StdMutex::new(String::new()));
        assert!(!r.contains(id), "unknown id must report not-contained");
        r.record(id, tx, buf, None);
        assert!(r.contains(id), "after record, contains true");
        r.forget(id);
        assert!(!r.contains(id), "after forget, contains false");
    }

    #[test]
    fn error_dto_serializes_with_kind_discriminant() {
        let dto = TerminalMeshErrorDto::from(&TerminalMeshError::NotFound {
            terminal_id: "abc".into(),
        });
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("notFound"));
        assert_eq!(v.get("terminalId").and_then(|x| x.as_str()), Some("abc"));
    }

    #[test]
    fn invalid_terminal_id_rejected() {
        match parse_terminal_id("not-a-uuid") {
            Err(TerminalMeshError::InvalidTerminalId { raw, .. }) => {
                assert_eq!(raw, "not-a-uuid");
            }
            other => panic!("expected InvalidTerminalId; got {other:?}"),
        }
    }

    #[test]
    fn append_scrollback_caps_at_retention_bytes() {
        let buf = StdArc::new(StdMutex::new(String::new()));
        // Push more than the cap and verify retention.
        let chunk = vec![b'A'; SCROLLBACK_RETENTION_BYTES + 4096];
        append_scrollback(&buf, &chunk);
        let len = buf.lock().unwrap().len();
        assert!(len <= SCROLLBACK_RETENTION_BYTES);
        assert!(len > SCROLLBACK_RETENTION_BYTES - 16);
    }

    #[test]
    fn append_scrollback_preserves_utf8_boundary_on_truncation() {
        let buf = StdArc::new(StdMutex::new(String::new()));
        // Build a payload that ends exactly at a multi-byte codepoint
        // straddling the cap boundary.
        let big: String = std::iter::repeat('A')
            .take(SCROLLBACK_RETENTION_BYTES + 2)
            .chain(std::iter::once('你'))
            .collect();
        append_scrollback(&buf, big.as_bytes());
        let s = buf.lock().unwrap().clone();
        // Round-trip must remain valid UTF-8 (String guarantees this);
        // explicitly check the trailing codepoint is intact.
        assert!(s.ends_with('你'), "trailing codepoint should be preserved; got {s:?}");
    }

    #[test]
    fn spawn_response_serializes_camel_case() {
        let r = TerminalSpawnResponse {
            terminal_id: "abc".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v.get("terminalId").and_then(|x| x.as_str()), Some("abc"));
    }

    /// Codex round-25 verification path: prove the registry sustains
    /// at least 4 simultaneous PTY sessions until explicit shutdown,
    /// mirroring the spawn-into-registry path that the Tauri command
    /// `terminal_spawn` walks. The full Tauri command requires an
    /// `AppHandle` which isn't constructible in a unit test, so we
    /// exercise the registry contract directly using the same
    /// underlying `TerminalActor::spawn` + `registry.record` calls.
    /// AC-4.1 ≥4 PTYs HARD at the user-visible layer reduces to this
    /// invariant: the registry must hold every spawned session until
    /// the user explicitly closes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn registry_supports_at_least_four_simultaneous_sessions() {
        use std::path::PathBuf;
        use terminal_mesh_core::{TerminalActor, TerminalHandle, TerminalSpec};

        let registry = TerminalMeshRegistry::new();
        // Keep the receivers alive so the actor's send_attention paths
        // don't backpressure during the test; we don't drain them.
        let mut keepalive_receivers: Vec<mpsc::Receiver<_>> = Vec::new();
        let mut keepalive_statuses: Vec<mpsc::Receiver<_>> = Vec::new();
        let mut command_txs: Vec<(uuid::Uuid, mpsc::Sender<ActorCommand>)> = Vec::new();

        for _ in 0..4 {
            let id = uuid::Uuid::new_v4();
            let handle = TerminalActor::spawn(TerminalSpec {
                terminal_id: id,
                command: PathBuf::from("/bin/sh"),
                args: vec!["-c".into(), "sleep 30".into()],
                cwd: None,
                env: vec![],
                cols: 80,
                rows: 24,
            })
            .expect("spawn");
            let TerminalHandle {
                events_rx,
                command_tx,
                status_rx,
                ..
            } = handle;
            let scrollback = StdArc::new(StdMutex::new(String::new()));
            registry.record(id, command_tx.clone(), scrollback, None);
            keepalive_receivers.push(events_rx);
            keepalive_statuses.push(status_rx);
            command_txs.push((id, command_tx));
        }

        assert_eq!(
            registry.active_count(),
            4,
            "all 4 sessions must remain registered concurrently"
        );
        for (id, _) in &command_txs {
            assert!(
                registry.lookup_command_tx(*id).is_some(),
                "session {id} should still be addressable until explicit shutdown"
            );
        }

        // Explicit shutdown for each session; the registry forgets
        // each entry the same way the Tauri command path does.
        for (id, tx) in &command_txs {
            let _ = tx.send(ActorCommand::Shutdown).await;
            registry.forget(*id);
        }
        assert_eq!(registry.active_count(), 0);
        // Receivers are kept alive until function end so the actor
        // tasks don't see channel-closed mid-test.
        drop(keepalive_receivers);
        drop(keepalive_statuses);
    }

    // ----- task21 / AC-3.3: cross-tab read privileged capability -----

    /// Helper: stand up a TerminalMeshRegistry with one terminal whose
    /// scrollback contains `body`. Returns (registry, terminal_id).
    fn registry_with_one_scrollback(body: &str) -> (TerminalMeshRegistry, Uuid) {
        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let buf = StdArc::new(StdMutex::new(body.to_string()));
        registry.record(id, tx, buf, None);
        (registry, id)
    }

    /// Like `registry_with_one_scrollback` but also indexes the
    /// terminal under `tab_id` so `lookup_terminal_by_tab` resolves.
    fn registry_with_tab_scrollback(
        body: &str,
        tab_id: &str,
    ) -> (TerminalMeshRegistry, Uuid) {
        let registry = TerminalMeshRegistry::new();
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<ActorCommand>(1);
        let buf = StdArc::new(StdMutex::new(body.to_string()));
        registry.record(id, tx, buf, Some(tab_id.to_string()));
        (registry, id)
    }

    #[test]
    fn cross_tab_read_returns_scrollback_when_cross_tab_read_flag_true() {
        let (term_registry, target_id) = registry_with_one_scrollback("hello world");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            Some("tab-orch"),
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let out = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &resp.handle,
            &target_id.to_string(),
            8192,
        )
        .expect("orchestrator handle can read scrollback");
        assert_eq!(out, "hello world");
    }

    #[test]
    fn cross_tab_read_returns_permission_denied_when_flag_false() {
        // AC-3.3 negative test: regular-tab handle is rejected even when
        // pointed at the same target. (Mount example-notes as a stand-in
        // for "any manifested non-orchestrator plugin"; the auth check
        // matches plugin_id="terminal-mesh", so use the orchestrator
        // path to set up a deliberately wrong-plugin handle below.)
        let (term_registry, target_id) = registry_with_one_scrollback("secret");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        // Mount terminal-mesh WITHOUT the privileged flag via the
        // generic insert_mount path (mirrors what a regular tab's
        // terminal-mesh capability would look like if one were ever
        // minted; today regular tabs don't mount terminal-mesh, but
        // the auth check must still deny if they did).
        let mount_id = Uuid::new_v4();
        let nonce = crate::dispatcher::fresh_nonce_bytes();
        let handle = crate::dispatcher::encode_handle(mount_id, &nonce);
        crate::dispatcher::insert_mount(
            &mount_registry,
            "terminal-mesh",
            mount_id,
            crate::dispatcher::hash_nonce(&nonce),
            vec![],
            Some("tab-regular".into()),
        );

        let err = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &handle,
            &target_id.to_string(),
            8192,
        )
        .unwrap_err();
        match err {
            TerminalMeshError::PermissionDenied { message } => {
                assert!(
                    message.contains("cross-tab read privilege"),
                    "unexpected permission_denied message: {message}"
                );
            }
            other => panic!("expected PermissionDenied; got {other:?}"),
        }
    }

    #[test]
    fn cross_tab_read_returns_at_most_hard_max_bytes() {
        // Pre-populate a scrollback larger than the hard cap; request
        // an absurdly-large max_bytes; assert the returned slice is
        // exactly MAX_CROSS_TAB_READ_BYTES (UTF-8 safe at the start).
        let big = "A".repeat(MAX_CROSS_TAB_READ_BYTES + 4096);
        let (term_registry, target_id) = registry_with_one_scrollback(&big);
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            None,
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let out = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &resp.handle,
            &target_id.to_string(),
            usize::MAX,
        )
        .expect("orchestrator handle reads with hard cap");
        assert_eq!(out.len(), MAX_CROSS_TAB_READ_BYTES);
        assert!(out.is_char_boundary(0));
    }

    #[test]
    fn cross_tab_read_returns_at_most_caller_requested_max_bytes() {
        // Caller-requested < hard cap → caller-requested wins.
        let body = "B".repeat(64 * 1024);
        let (term_registry, target_id) = registry_with_one_scrollback(&body);
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            None,
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let out = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &resp.handle,
            &target_id.to_string(),
            4096,
        )
        .expect("orchestrator handle reads with caller cap");
        assert!(out.len() <= 4096, "got {} bytes", out.len());
    }

    #[test]
    fn cross_tab_read_returns_not_found_for_unknown_target() {
        let term_registry = TerminalMeshRegistry::new();
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            None,
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let unknown = Uuid::new_v4();
        let err = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &resp.handle,
            &unknown.to_string(),
            8192,
        )
        .unwrap_err();
        match err {
            TerminalMeshError::NotFound { terminal_id } => {
                assert_eq!(terminal_id, unknown.to_string());
            }
            other => panic!("expected NotFound; got {other:?}"),
        }
    }

    #[test]
    fn cross_tab_read_returns_permission_denied_for_malformed_handle() {
        let (term_registry, target_id) = registry_with_one_scrollback("x");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let err = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            "not-a-handle",
            &target_id.to_string(),
            8192,
        )
        .unwrap_err();
        assert!(matches!(err, TerminalMeshError::PermissionDenied { .. }));
    }

    // ----- task21 Round 38 remediation: tab-id index + permission gate -----

    #[test]
    fn lookup_terminal_by_tab_resolves_to_recorded_terminal_id() {
        let (registry, terminal_id) = registry_with_tab_scrollback("hello", "tab-A");
        assert_eq!(
            registry.lookup_terminal_by_tab("tab-A"),
            Some(terminal_id)
        );
        assert_eq!(registry.lookup_terminal_by_tab("tab-missing"), None);
    }

    #[test]
    fn forget_removes_tab_id_index_entry() {
        let (registry, terminal_id) = registry_with_tab_scrollback("hello", "tab-A");
        assert!(registry.lookup_terminal_by_tab("tab-A").is_some());
        registry.forget(terminal_id);
        assert!(
            registry.lookup_terminal_by_tab("tab-A").is_none(),
            "tab_index must be cleaned up alongside the primary entry"
        );
    }

    #[test]
    fn cross_tab_read_by_tab_resolves_via_tab_index() {
        let (term_registry, _) = registry_with_tab_scrollback("body-by-tab", "tab-X");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            None,
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let out = cross_tab_read_by_tab(
            &mount_registry,
            &term_registry,
            &resp.handle,
            "tab-X",
            8192,
        )
        .expect("orchestrator can read tab-X via tab id");
        assert_eq!(out, "body-by-tab");
    }

    #[test]
    fn cross_tab_read_by_tab_returns_not_found_for_unknown_tab() {
        let (term_registry, _) = registry_with_tab_scrollback("x", "tab-known");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            None,
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let err = cross_tab_read_by_tab(
            &mount_registry,
            &term_registry,
            &resp.handle,
            "tab-unknown",
            8192,
        )
        .unwrap_err();
        assert!(matches!(err, TerminalMeshError::NotFound { .. }));
    }

    #[test]
    fn cross_tab_read_inner_denies_when_entry_missing_cross_tab_read_permission() {
        // Synthetic mount with `cross_tab_read_flag = true` BUT empty
        // permissions vec — must still be denied at read time. Defense
        // in depth: even if the privileged grant was minted out of
        // band, the read endpoint requires the declared permission.
        let (term_registry, target_id) = registry_with_one_scrollback("secret");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let mount_id = Uuid::new_v4();
        let nonce = crate::dispatcher::fresh_nonce_bytes();
        let handle = crate::dispatcher::encode_handle(mount_id, &nonce);
        crate::dispatcher::insert_mount_with_flag(
            &mount_registry,
            "terminal-mesh",
            mount_id,
            crate::dispatcher::hash_nonce(&nonce),
            vec![],        // <-- no permissions
            Some("tab-evil".into()),
            true,          // <-- flag is true; permission is missing
        );
        let err = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &handle,
            &target_id.to_string(),
            8192,
        )
        .unwrap_err();
        match err {
            TerminalMeshError::PermissionDenied { message } => {
                assert!(
                    message.contains("does not declare `cross_tab_read`"),
                    "unexpected permission_denied message: {message}"
                );
                assert!(
                    message.contains("terminal-mesh.read_scrollback"),
                    "message should name the command path: {message}"
                );
            }
            other => panic!("expected PermissionDenied; got {other:?}"),
        }
    }

    #[test]
    fn cross_tab_read_inner_denies_when_pty_read_scrollback_permission_missing() {
        // Mount has cross_tab_read_flag=true AND declares cross_tab_read
        // BUT misses pty.read_scrollback (the second command-level
        // requirement added in Round 39). The read MUST be denied.
        let (term_registry, target_id) = registry_with_one_scrollback("secret-2");
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let mount_id = Uuid::new_v4();
        let nonce = crate::dispatcher::fresh_nonce_bytes();
        let handle = crate::dispatcher::encode_handle(mount_id, &nonce);
        crate::dispatcher::insert_mount_with_flag(
            &mount_registry,
            "terminal-mesh",
            mount_id,
            crate::dispatcher::hash_nonce(&nonce),
            vec!["cross_tab_read".into()], // <-- missing pty.read_scrollback
            Some("tab-half-perm".into()),
            true,
        );
        let err = cross_tab_read_inner(
            &mount_registry,
            &term_registry,
            &handle,
            &target_id.to_string(),
            8192,
        )
        .unwrap_err();
        match err {
            TerminalMeshError::PermissionDenied { message } => {
                assert!(
                    message.contains("does not declare `pty.read_scrollback`"),
                    "expected pty.read_scrollback denial; got: {message}"
                );
            }
            other => panic!("expected PermissionDenied; got {other:?}"),
        }
    }

    #[test]
    fn check_command_permissions_denies_when_command_metadata_missing() {
        // A privileged mount whose declared permissions are fine
        // for terminal-mesh.read_scrollback should STILL deny if
        // we ask about a command the metadata doesn't know about.
        let mount_registry = crate::dispatcher::MountRegistry::new();
        let resp = crate::dispatcher::insert_orchestrator_mount(
            &mount_registry,
            "terminal-mesh",
            None,
        )
        .expect("terminal-mesh built-in metadata declares cross_tab_read");
        let entry = crate::dispatcher::authorize_capability_handle(
            &mount_registry,
            &resp.handle,
            "terminal-mesh",
        )
        .expect("handle authorizes");
        let err = check_command_permissions(&entry, "terminal-mesh", "no-such-command").unwrap_err();
        match err {
            TerminalMeshError::PermissionDenied { message } => {
                assert!(
                    message.contains("command metadata missing"),
                    "expected metadata-missing denial; got: {message}"
                );
            }
            other => panic!("expected PermissionDenied; got {other:?}"),
        }
    }

    #[test]
    fn read_tab_scrollback_bounded_returns_scrollback_for_known_tab() {
        let (term_registry, _) = registry_with_tab_scrollback("self-read", "tab-self");
        let out = read_tab_scrollback_bounded(&term_registry, "tab-self", 8192)
            .expect("self-read for known tab works without capability");
        assert_eq!(out, "self-read");
    }

    #[test]
    fn read_tab_scrollback_bounded_returns_not_found_for_unknown_tab() {
        let term_registry = TerminalMeshRegistry::new();
        let err = read_tab_scrollback_bounded(&term_registry, "tab-missing", 8192).unwrap_err();
        assert!(matches!(err, TerminalMeshError::NotFound { .. }));
    }
}
