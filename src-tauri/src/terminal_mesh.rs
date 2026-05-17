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
use tauri::{AppHandle, Emitter, State};
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSpawnResponse {
    pub terminal_id: String,
}

struct TerminalSession {
    command_tx: mpsc::Sender<ActorCommand>,
    scrollback: Arc<StdMutex<String>>,
}

/// Tauri-managed registry of live terminal sessions.
pub struct TerminalMeshRegistry {
    inner: StdMutex<HashMap<Uuid, TerminalSession>>,
}

impl Default for TerminalMeshRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalMeshRegistry {
    pub fn new() -> Self {
        Self {
            inner: StdMutex::new(HashMap::new()),
        }
    }

    fn record(
        &self,
        id: Uuid,
        command_tx: mpsc::Sender<ActorCommand>,
        scrollback: Arc<StdMutex<String>>,
    ) {
        let mut guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.insert(
            id,
            TerminalSession {
                command_tx,
                scrollback,
            },
        );
    }

    fn lookup_command_tx(&self, id: Uuid) -> Option<mpsc::Sender<ActorCommand>> {
        let guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.get(&id).map(|s| s.command_tx.clone())
    }

    fn lookup_scrollback(&self, id: Uuid) -> Option<Arc<StdMutex<String>>> {
        let guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.get(&id).map(|s| s.scrollback.clone())
    }

    fn forget(&self, id: Uuid) {
        let mut guard = self.inner.lock().expect("TerminalMeshRegistry poisoned");
        guard.remove(&id);
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

    let terminal_id = spawn_into_registry(spec, &app, &registry)
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
) -> Result<Uuid, TerminalMeshError> {
    let handle = TerminalActor::spawn(spec).map_err(TerminalMeshError::from)?;
    let TerminalHandle {
        terminal_id: id,
        events_rx,
        command_tx,
        status_rx,
    } = handle;
    let scrollback = Arc::new(StdMutex::new(String::new()));
    registry.record(id, command_tx, scrollback.clone());

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
        if let Err(err) = app.emit(&topic, &env) {
            tracing::warn!(%id, %err, "terminal event emit failed");
        }
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
        r.record(id, tx.clone(), buf.clone());
        assert!(r.lookup_command_tx(id).is_some());
        assert!(r.lookup_scrollback(id).is_some());
        assert_eq!(r.active_count(), 1);
        r.forget(id);
        assert!(r.lookup_command_tx(id).is_none());
        assert_eq!(r.active_count(), 0);
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
            registry.record(id, command_tx.clone(), scrollback);
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
}
