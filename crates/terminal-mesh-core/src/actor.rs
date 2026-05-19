//! Tokio actor wrapping a `portable-pty` child process.
//!
//! Architecture:
//! - One `std::thread` runs the synchronous PTY reader loop and
//!   forwards chunks over an mpsc to the processor task.
//! - One `tokio::task` runs the processor: feeds the ring buffer +
//!   OSC parser + prompt detector, routes `NeedsAttention` through
//!   the `DedupArbiter`, and emits `TerminalEventEnvelope`s.
//! - One `tokio::task` runs the command listener: writes stdin,
//!   resizes the master PTY, and orchestrates shutdown.
//! - One `tokio::task::spawn_blocking` waits on `Child::wait()` and
//!   signals exit through a oneshot.
//!
//! Spec: `docs/specs/terminal-events.md`. Targets AC-4.1 + AC-4.2.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::events::{
    dedup_key, AttentionKind, BufferTruncated, NeedsAttentionPayload, TerminalEvent,
    TerminalEventEnvelope, TERMINAL_MESH_PLUGIN_ID,
};
use crate::osc_agent_marker::{OscAttentionEvent, OscAttentionParser};
use crate::prompt_detector::PromptDetector;
use crate::ring_buffer::RingBuffer;
use crate::transport::{
    LocalTransport, PathBufOrRemote, PtySize as TransportPtySize, ShellCommand, ShutdownMode,
    Transport, TransportExitStatus, TransportResizeHandle, TransportShutdownHandle,
    TransportSpawnRequest, TransportStdinSink, WorkspaceLocation,
};

const READ_CHUNK_BYTES: usize = 16 * 1024;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const STATUS_CHANNEL_CAPACITY: usize = 64;
const COMMAND_CHANNEL_CAPACITY: usize = 64;
const RAW_BYTES_CHANNEL_CAPACITY: usize = 64;
const PROMPT_TAIL_BYTES: usize = 4096;
const PROMPT_TICK_INTERVAL: Duration = Duration::from_millis(200);
/// Post-SIGHUP grace window before SIGKILL escalation. portable-pty's
/// `ProcessSignaller::kill()` only sends SIGHUP on Unix; this gives a
/// cooperative child time to flush + exit before we hard-kill it. Set
/// at the midpoint of Codex's 300-500ms recommendation.
const ESCALATE_TO_SIGKILL_AFTER: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
pub struct TerminalSpec {
    pub terminal_id: Uuid,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    /// Optional override for the `TransportSpawnRequest.workspace`
    /// field. When `None`, `TerminalActor::spawn` defaults to
    /// `WorkspaceLocation::Local { path: cwd }` (baseline behavior).
    /// When `Some(loc)`, the spec author is responsible for picking
    /// a matching transport — typically by routing through one of
    /// `LocalTransport` / `SshTransport` / `DockerOverSshTransport`.
    /// Threaded by the production auto-launch path so Remote /
    /// Docker workspaces reach the transport-side branch picker.
    pub workspace_location: Option<WorkspaceLocation>,
}

#[derive(Debug)]
pub enum ActorCommand {
    WriteStdin(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Shutdown,
}

#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("PTY error: {0}")]
    Pty(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("actor already shut down")]
    AlreadyShutdown,
}

pub struct TerminalHandle {
    pub terminal_id: Uuid,
    pub events_rx: mpsc::Receiver<TerminalEventEnvelope>,
    pub command_tx: mpsc::Sender<ActorCommand>,
    pub status_rx: mpsc::Receiver<BufferTruncated>,
}

/// Outcome of `TransportSession::wait` after collapsing the
/// transport's typed exit status into the discrete signals the
/// actor's processor task cares about. The wait thread maps:
///
/// - `CleanCompletion` / `NonZeroExit(N)` / `Signaled(N)` → `Completed(N)`
///   (CleanCompletion is `Completed(0)`).
/// - `Disconnect(_)` from the transport → `Disconnected`.
/// - `wait()` returning `Err(_)` → `Disconnected`.
///
/// The processor then translates `Disconnected` into both a
/// `TerminalEvent::Exit { code: None }` AND a
/// `NeedsAttention(AttentionKind::Disconnect)` event, so the host's
/// lifecycle layer can map a lost remote channel to
/// `DoneReason::Disconnected`.
#[derive(Debug, Clone, Copy)]
enum WaitResult {
    Completed(i32),
    Disconnected,
}

pub struct TerminalActor;

impl TerminalActor {
    /// Spawn a Terminal Mesh actor backed by the supplied `transport`.
    /// The actor runs on the ambient Tokio runtime; the byte reader is a
    /// standalone `std::thread` so a blocking PTY read can't starve the
    /// runtime, and a second thread owns the session for the blocking
    /// `wait`. Resize and shutdown go through handles extracted from
    /// the session before `wait` takes ownership, so they remain
    /// dispatchable while wait is in progress.
    pub fn spawn(
        transport: Arc<dyn Transport>,
        spec: TerminalSpec,
    ) -> Result<TerminalHandle, ActorError> {
        let TerminalSpec {
            terminal_id,
            command,
            args,
            cwd,
            env,
            cols,
            rows,
            workspace_location,
        } = spec;

        let mut env_map: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in env {
            env_map.insert(k, v);
        }
        let workspace = workspace_location.unwrap_or_else(|| WorkspaceLocation::Local {
            path: cwd.clone(),
        });
        // For Local workspaces the legacy `cwd` field carries the
        // path; Remote workspaces consume `canonical_remote_path`
        // via the transport's own remote-cwd handling, so the
        // transport-side `cwd` slot becomes `None` for them.
        let transport_cwd = match &workspace {
            WorkspaceLocation::Local { .. } => cwd.map(PathBufOrRemote::Local),
            WorkspaceLocation::Remote { .. } => None,
        };
        let request = TransportSpawnRequest {
            workspace,
            command: ShellCommand {
                program: command,
                args,
            },
            initial_size: TransportPtySize { cols, rows },
            env: env_map,
            cwd: transport_cwd,
        };

        let mut session = transport
            .spawn(request)
            .map_err(|e| ActorError::Pty(e.to_string()))?;

        let mut reader = session
            .output_stream()
            .map_err(|e| ActorError::Pty(e.to_string()))?;
        let writer = session
            .stdin_sink()
            .map_err(|e| ActorError::Pty(e.to_string()))?;
        let shutdown_handle: Arc<dyn TransportShutdownHandle> = session
            .take_shutdown_handle()
            .ok_or_else(|| {
                ActorError::Pty(
                    "transport does not expose a concurrent shutdown handle".into(),
                )
            })?
            .into();
        let resize_handle: Arc<dyn TransportResizeHandle> = session
            .take_resize_handle()
            .ok_or_else(|| {
                ActorError::Pty(
                    "transport does not expose a concurrent resize handle".into(),
                )
            })?
            .into();

        let (events_tx, events_rx) = mpsc::channel::<TerminalEventEnvelope>(EVENT_CHANNEL_CAPACITY);
        let (status_tx, status_rx) = mpsc::channel::<BufferTruncated>(STATUS_CHANNEL_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel::<ActorCommand>(COMMAND_CHANNEL_CAPACITY);
        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(RAW_BYTES_CHANNEL_CAPACITY);
        let (reader_done_tx, reader_done_rx) = oneshot::channel::<()>();
        let (exit_tx, exit_rx) = oneshot::channel::<WaitResult>();

        // Reader thread (sync). Reads from the transport's output stream
        // until EOF or error; forwards every chunk over `bytes_tx`.
        // Signals `reader_done_tx` when finished so the processor can
        // drain pending bytes.
        std::thread::Builder::new()
            .name(format!("terminal-mesh-reader-{terminal_id}"))
            .spawn(move || {
                let mut buf = [0u8; READ_CHUNK_BYTES];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if bytes_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = reader_done_tx.send(());
            })
            .map_err(|e| ActorError::Io(e.to_string()))?;

        // Wait thread owns the residual session and calls the blocking
        // `wait`. Shutdown happens out-of-band via the pre-extracted
        // shutdown handle, so we don't need any Mutex/Arc indirection
        // around the session itself.
        std::thread::Builder::new()
            .name(format!("terminal-mesh-wait-{terminal_id}"))
            .spawn(move || {
                let outcome = match session.wait() {
                    Ok(TransportExitStatus::CleanCompletion) => WaitResult::Completed(0),
                    Ok(TransportExitStatus::NonZeroExit(n))
                    | Ok(TransportExitStatus::Signaled(n)) => WaitResult::Completed(n),
                    Ok(TransportExitStatus::Disconnect(_)) | Err(_) => WaitResult::Disconnected,
                };
                let _ = exit_tx.send(outcome);
            })
            .map_err(|e| ActorError::Io(e.to_string()))?;

        tokio::spawn(processor_task(
            terminal_id,
            resize_handle,
            writer,
            shutdown_handle,
            bytes_rx,
            command_rx,
            events_tx,
            status_tx,
            reader_done_rx,
            exit_rx,
        ));

        Ok(TerminalHandle {
            terminal_id,
            events_rx,
            command_tx,
            status_rx,
        })
    }

    /// Backward-compatible entry point that uses the in-process
    /// `LocalTransport`. This is the default path callers reach for when
    /// they do not need to inject an alternate transport.
    pub fn spawn_local(spec: TerminalSpec) -> Result<TerminalHandle, ActorError> {
        Self::spawn(Arc::new(LocalTransport::new()) as Arc<dyn Transport>, spec)
    }
}

#[allow(clippy::too_many_arguments)]
async fn processor_task(
    terminal_id: Uuid,
    resize_handle: Arc<dyn TransportResizeHandle>,
    writer_in: Box<dyn TransportStdinSink>,
    shutdown_handle: Arc<dyn TransportShutdownHandle>,
    mut bytes_rx: mpsc::Receiver<Vec<u8>>,
    mut command_rx: mpsc::Receiver<ActorCommand>,
    events_tx: mpsc::Sender<TerminalEventEnvelope>,
    status_tx: mpsc::Sender<BufferTruncated>,
    mut reader_done_rx: oneshot::Receiver<()>,
    mut exit_rx: oneshot::Receiver<WaitResult>,
) {
    // Wrap writer in Option so we can drop it after the child exits to
    // release the master fd refcount and help the kernel propagate EOF
    // to the reader thread.
    let mut writer: Option<Box<dyn TransportStdinSink>> = Some(writer_in);
    let mut ring = RingBuffer::new();
    let mut osc = OscAttentionParser::new();
    // Quiescence mode: fire PromptWaiting after 1 s of no new output.
    // This works for Claude Code's TUI (no shell-style $ prompt) and
    // regular shells alike — once output stops arriving, the agent
    // has finished its current task or is waiting for input.
    let mut prompt = PromptDetector::quiescence();
    let mut tick = tokio::time::interval(PROMPT_TICK_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown_requested = false;
    let mut child_outcome: Option<WaitResult> = None;
    let mut commands_closed = false;
    let mut bytes_closed = false;
    let mut reader_drained = false;
    let mut exit_observed = false;
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    // Set on Shutdown; if it fires before exit_rx resolves, the
    // processor escalates from SIGHUP (already issued via killer) to
    // SIGKILL on Unix. Cleared after escalation so the arm only
    // fires once per shutdown.
    let mut escalation_deadline: Option<tokio::time::Instant> = None;

    loop {
        // Termination predicate: we have observed the child exit AND
        // either drained the reader's pending bytes OR exhausted the
        // post-exit grace period (some PTYs don't always EOF cleanly
        // when the master writer is still held by another reference).
        if exit_observed
            && (bytes_closed
                || drain_deadline.is_some_and(|d| tokio::time::Instant::now() >= d))
        {
            break;
        }

        tokio::select! {
            // ---- child exited ----
            outcome = &mut exit_rx, if !exit_observed => {
                // The wait thread may have been dropped before sending
                // (process aborted, channel closed). Treat that as a
                // Disconnected outcome so the post-exit drain still
                // emits the Disconnect attention path.
                child_outcome = Some(outcome.unwrap_or(WaitResult::Disconnected));
                exit_observed = true;
                // Drop the writer so the master fd refcount goes down,
                // helping the kernel propagate EOF to the reader.
                writer = None;
                drain_deadline = Some(
                    tokio::time::Instant::now() + Duration::from_millis(250),
                );
            }

            // ---- commands from the caller ----
            cmd = command_rx.recv(), if !commands_closed => {
                match cmd {
                    Some(ActorCommand::WriteStdin(bytes)) => {
                        if let Some(w) = writer.as_mut() {
                            if let Err(e) = write_all_sink(w.as_mut(), &bytes) {
                                tracing::warn!(%terminal_id, %e, "stdin write failed");
                            }
                            let _ = w.flush();
                        }
                    }
                    Some(ActorCommand::Resize { cols, rows }) => {
                        if let Err(e) = resize_handle
                            .resize(TransportPtySize { cols, rows })
                        {
                            tracing::warn!(%terminal_id, %e, "pty resize failed");
                        }
                        let envelope = TerminalEventEnvelope::now(
                            terminal_id,
                            TerminalEvent::Resize { cols, rows },
                        );
                        let _ = events_tx.send(envelope).await;
                    }
                    Some(ActorCommand::Shutdown) => {
                        shutdown_requested = true;
                        // Cooperative shutdown first — SIGHUP on Unix
                        // via the pre-extracted handle. A well-behaved
                        // child unwinds and exits, and the wait thread
                        // then resolves `exit_rx` on its own.
                        if let Err(e) = shutdown_handle.shutdown(ShutdownMode::Graceful) {
                            tracing::warn!(%terminal_id, %e, "graceful shutdown failed");
                        }
                        // Arm the escalation deadline; if `exit_rx`
                        // does not resolve within the grace window,
                        // the dedicated select arm below sends a hard
                        // kill via the same handle.
                        if escalation_deadline.is_none() && !exit_observed {
                            escalation_deadline = Some(
                                tokio::time::Instant::now() + ESCALATE_TO_SIGKILL_AFTER,
                            );
                        }
                    }
                    None => {
                        // Caller dropped the command sender. Don't
                        // treat as Shutdown — the child may still be
                        // running normally and we want to surface its
                        // natural Exit. Just stop polling this arm.
                        commands_closed = true;
                    }
                }
            }

            // ---- bytes from the reader thread ----
            chunk = bytes_rx.recv(), if !bytes_closed => {
                match chunk {
                    Some(bytes) => {
                        let info = ring.push(&bytes);
                        if let Some(info) = info {
                            let _ = status_tx
                                .send(BufferTruncated {
                                    terminal_id,
                                    plugin_id: TERMINAL_MESH_PLUGIN_ID.to_string(),
                                    timestamp: std::time::SystemTime::now(),
                                    bytes_dropped: info.bytes_dropped,
                                })
                                .await;
                        }
                        for ev in osc.feed(&bytes) {
                            let kind = match ev {
                                OscAttentionEvent::AgentMarker(m) => {
                                    AttentionKind::AgentMarker {
                                        summary: m.summary,
                                        severity: m.severity,
                                    }
                                }
                                OscAttentionEvent::TaskComplete { summary } => {
                                    AttentionKind::TaskComplete { summary }
                                }
                            };
                            emit_attention(terminal_id, &events_tx, kind).await;
                        }
                        let tail = ring.read_scrollback(PROMPT_TAIL_BYTES);
                        if prompt.poll(&tail, Instant::now()).is_some() {
                            emit_attention(
                                terminal_id,
                                &events_tx,
                                AttentionKind::PromptWaiting,
                            )
                            .await;
                        }
                        let envelope = TerminalEventEnvelope::now(
                            terminal_id,
                            TerminalEvent::Output { bytes },
                        );
                        let _ = events_tx.send(envelope).await;
                    }
                    None => {
                        bytes_closed = true;
                    }
                }
            }

            // ---- periodic prompt tick (no new bytes case) ----
            _ = tick.tick() => {
                if prompt.tick(Instant::now()).is_some() {
                    emit_attention(
                        terminal_id,
                        &events_tx,
                        AttentionKind::PromptWaiting,
                    )
                    .await;
                }
            }

            // ---- reader thread finished ----
            _ = &mut reader_done_rx, if !reader_drained => {
                reader_drained = true;
            }

            // ---- SIGKILL escalation for HUP-trapping children ----
            _ = async {
                if let Some(d) = escalation_deadline {
                    tokio::time::sleep_until(d).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if escalation_deadline.is_some() && !exit_observed => {
                if let Err(e) = shutdown_handle.shutdown(ShutdownMode::Kill) {
                    tracing::warn!(%terminal_id, %e, "hard kill escalation failed");
                }
                // Single-shot: clear the deadline so the arm doesn't
                // fire repeatedly. If the hard kill itself somehow
                // fails to terminate the child, the wait thread stays
                // blocked and the loop falls through to the
                // drain-deadline exit path on the next iteration anyway.
                escalation_deadline = None;
            }

            // ---- post-exit drain deadline ----
            _ = async {
                if let Some(d) = drain_deadline {
                    tokio::time::sleep_until(d).await;
                } else {
                    // Never resolve when there's no deadline.
                    std::future::pending::<()>().await;
                }
            }, if drain_deadline.is_some() => {
                // Deadline reached; the outer loop predicate will
                // break on the next iteration.
                drain_deadline = Some(tokio::time::Instant::now());
            }
        }
    }

    // Map the wait outcome into the public event sequence. Note
    // that `child_outcome == None` here means the drain deadline
    // expired without a wait signal — emit `Exit { code: None }`
    // with no attention event (the host's notification service has
    // no signal to act on either way).
    let (exit_code, attention_kind) = match child_outcome {
        Some(WaitResult::Completed(c)) => {
            let kind = if c == 0 {
                AttentionKind::Completion { exit_code: 0 }
            } else {
                AttentionKind::NonZeroExit { exit_code: c }
            };
            (Some(c), Some(kind))
        }
        Some(WaitResult::Disconnected) => (None, Some(AttentionKind::Disconnect)),
        None => (None, None),
    };
    let envelope =
        TerminalEventEnvelope::now(terminal_id, TerminalEvent::Exit { code: exit_code });
    let _ = events_tx.send(envelope).await;
    // Host `NotificationService` owns notification dedup; emit
    // attention unconditionally when the wait outcome implies one.
    if let Some(kind) = attention_kind {
        emit_attention(terminal_id, &events_tx, kind).await;
    }
    if shutdown_requested {
        let env = TerminalEventEnvelope::now(terminal_id, TerminalEvent::Cancelled);
        let _ = events_tx.send(env).await;
    }
}

/// Drain `bytes` into `sink` using repeated `write` calls until either
/// all bytes are written or the sink returns an error. `TransportStdinSink`
/// does not extend `std::io::Write`, so we cannot rely on `write_all`.
fn write_all_sink(
    sink: &mut dyn TransportStdinSink,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        match sink.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "transport stdin sink wrote zero bytes",
                ));
            }
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Emit every classified `NeedsAttention` event unconditionally; the
/// host `NotificationService` owns dedup. Each call generates a fresh
/// `event_id` so the host arbiter can distinguish individual events
/// for its suppressed-count tracking.
async fn emit_attention(
    terminal_id: Uuid,
    events_tx: &mpsc::Sender<TerminalEventEnvelope>,
    kind: AttentionKind,
) {
    let key = dedup_key(TERMINAL_MESH_PLUGIN_ID, terminal_id, &kind);
    let event_id = Uuid::new_v4();
    let payload = NeedsAttentionPayload {
        event_id,
        dedup_key: key,
        kind,
    };
    let env = TerminalEventEnvelope::now(
        terminal_id,
        TerminalEvent::NeedsAttention { payload },
    );
    let _ = events_tx.send(env).await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::transport::{
        Transport, TransportError, TransportOutputStream, TransportResizeHandle, TransportSession,
        TransportShutdownHandle, TransportSpawnRequest, TransportStdinSink,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn shell_spec(script: &str) -> TerminalSpec {
        TerminalSpec {
            terminal_id: Uuid::new_v4(),
            command: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            workspace_location: None,
        }
    }

    /// Collect events until `cond(envelope)` returns true OR timeout.
    async fn collect_until<F: FnMut(&TerminalEventEnvelope) -> bool>(
        rx: &mut mpsc::Receiver<TerminalEventEnvelope>,
        mut cond: F,
        timeout_ms: u64,
    ) -> Vec<TerminalEventEnvelope> {
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(env)) => {
                    let done = cond(&env);
                    events.push(env);
                    if done {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        events
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_emits_output_then_exit_for_echo_command() {
        let handle = TerminalActor::spawn_local(shell_spec("echo hi; exit 0")).expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = handle;
        let events = collect_until(
            &mut events_rx,
            |e| matches!(e.event, TerminalEvent::Exit { .. }),
            5000,
        )
        .await;
        let has_output = events.iter().any(|e| matches!(e.event, TerminalEvent::Output { .. }));
        let has_exit = events.iter().any(|e| matches!(e.event, TerminalEvent::Exit { .. }));
        assert!(has_output, "must emit at least one Output envelope");
        assert!(has_exit, "must emit an Exit envelope");
        let _ = command_tx; // keep tx alive until here so the actor doesn't see channel-close as shutdown
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_emits_completion_for_zero_exit_and_nonzero_for_nonzero_exit() {
        let h0 = TerminalActor::spawn_local(shell_spec("exit 0")).expect("spawn");
        let mut rx0 = h0.events_rx;
        let _tx0 = h0.command_tx;
        let evs0 = collect_until(
            &mut rx0,
            |e| matches!(
                e.event,
                TerminalEvent::NeedsAttention {
                    payload: NeedsAttentionPayload {
                        kind: AttentionKind::Completion { .. },
                        ..
                    },
                }
            ),
            5000,
        )
        .await;
        assert!(evs0.iter().any(|e| matches!(
            e.event,
            TerminalEvent::NeedsAttention {
                payload: NeedsAttentionPayload {
                    kind: AttentionKind::Completion { exit_code: 0 },
                    ..
                },
            }
        )));

        let h1 = TerminalActor::spawn_local(shell_spec("exit 7")).expect("spawn");
        let mut rx1 = h1.events_rx;
        let _tx1 = h1.command_tx;
        let evs1 = collect_until(
            &mut rx1,
            |e| matches!(
                e.event,
                TerminalEvent::NeedsAttention {
                    payload: NeedsAttentionPayload {
                        kind: AttentionKind::NonZeroExit { .. },
                        ..
                    },
                }
            ),
            5000,
        )
        .await;
        assert!(evs1.iter().any(|e| matches!(
            e.event,
            TerminalEvent::NeedsAttention {
                payload: NeedsAttentionPayload {
                    kind: AttentionKind::NonZeroExit { exit_code: 7 },
                    ..
                },
            }
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_propagates_stdin_write_to_child() {
        // cat reads stdin and echoes; we write "abc\n", exit on EOF.
        // Use `head -1` to bound the test instead of sending EOF.
        let h = TerminalActor::spawn_local(shell_spec("head -1")).expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = h;
        command_tx
            .send(ActorCommand::WriteStdin(b"abc\n".to_vec()))
            .await
            .unwrap();
        let evs = collect_until(
            &mut events_rx,
            |e| matches!(e.event, TerminalEvent::Exit { .. }),
            5000,
        )
        .await;
        // Some Output envelope must contain "abc".
        let mut all_bytes: Vec<u8> = Vec::new();
        for e in &evs {
            if let TerminalEvent::Output { bytes } = &e.event {
                all_bytes.extend_from_slice(bytes);
            }
        }
        let s = String::from_utf8_lossy(&all_bytes);
        assert!(s.contains("abc"), "stdin echo not seen; got {s:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_handles_resize_command_without_crash() {
        let h = TerminalActor::spawn_local(shell_spec("sleep 0.5")).expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = h;
        command_tx
            .send(ActorCommand::Resize { cols: 120, rows: 40 })
            .await
            .unwrap();
        let evs = collect_until(
            &mut events_rx,
            |e| matches!(e.event, TerminalEvent::Exit { .. }),
            5000,
        )
        .await;
        let saw_resize = evs.iter().any(|e| matches!(
            e.event,
            TerminalEvent::Resize { cols: 120, rows: 40 }
        ));
        assert!(saw_resize, "Resize envelope not observed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_cwd_and_env_are_propagated_to_child() {
        let tmp = tempfile_dir();
        let h = TerminalActor::spawn_local(TerminalSpec {
            terminal_id: Uuid::new_v4(),
            command: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "echo CWD=$(pwd) MY=$MY_VAR; exit 0".into()],
            cwd: Some(tmp.clone()),
            env: vec![("MY_VAR".into(), "hello".into())],
            cols: 80,
            rows: 24,
            workspace_location: None,
        })
        .expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = h;
        let _keep = command_tx;
        let evs = collect_until(
            &mut events_rx,
            |e| matches!(e.event, TerminalEvent::Exit { .. }),
            5000,
        )
        .await;
        let mut buf = Vec::new();
        for e in &evs {
            if let TerminalEvent::Output { bytes } = &e.event {
                buf.extend_from_slice(bytes);
            }
        }
        let s = String::from_utf8_lossy(&buf);
        // macOS sometimes resolves cwd through a /private symlink, so
        // compare canonical paths.
        let canonical = std::fs::canonicalize(&tmp).unwrap_or(tmp.clone());
        assert!(
            s.contains(&canonical.display().to_string()) || s.contains(&tmp.display().to_string()),
            "cwd not seen in child output; output={s:?}"
        );
        assert!(s.contains("MY=hello"), "env var not propagated; output={s:?}");
    }

    fn tempfile_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("tm-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Codex round-22 blocker #1 regression: a long-running PTY child
    /// must terminate promptly when Shutdown is sent — proving the
    /// ChildKiller path no longer races the wait-thread's blocking
    /// `child.wait()` for a held mutex.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_kills_long_running_child_and_emits_cancelled() {
        let started = std::time::Instant::now();
        let h = TerminalActor::spawn_local(shell_spec("sleep 30")).expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = h;
        // Give the child a moment to actually start.
        tokio::time::sleep(Duration::from_millis(80)).await;
        command_tx.send(ActorCommand::Shutdown).await.unwrap();
        let evs = collect_until(
            &mut events_rx,
            |e| matches!(e.event, TerminalEvent::Cancelled),
            3000,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown must complete well before the 30s natural exit; took {elapsed:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e.event, TerminalEvent::Exit { .. })),
            "Exit envelope expected after killer.kill(); got {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e.event, TerminalEvent::Cancelled)),
            "Cancelled envelope expected after Shutdown; got {evs:?}"
        );
    }

    /// Codex round-23 blocker regression: portable-pty's
    /// `ChildKiller::kill()` only sends SIGHUP on Unix. A child that
    /// installs `trap "" HUP` ignores SIGHUP entirely, so without
    /// SIGKILL escalation the wait thread blocks until natural exit.
    /// With escalation, `Shutdown` must terminate the child within
    /// the ~400ms grace + reasonable cleanup margin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_escalates_to_sigkill_for_hup_trapping_child() {
        let started = std::time::Instant::now();
        let h = TerminalActor::spawn_local(shell_spec("trap '' HUP; sleep 30"))
            .expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = h;
        // Give the child time to install the trap and start sleeping.
        tokio::time::sleep(Duration::from_millis(150)).await;
        command_tx.send(ActorCommand::Shutdown).await.unwrap();
        let evs = collect_until(
            &mut events_rx,
            |e| matches!(e.event, TerminalEvent::Cancelled),
            3000,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "SIGKILL escalation must close the HUP-ignoring child well \
             before its 30s natural exit; took {elapsed:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e.event, TerminalEvent::Exit { .. })),
            "Exit envelope expected after SIGKILL; got {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e.event, TerminalEvent::Cancelled)),
            "Cancelled envelope expected after Shutdown; got {evs:?}"
        );
    }

    /// Task16 stress proof for AC-4.1's ≥4 PTYs HARD requirement
    /// under synthetic fast-output load: 8 actors each emit ~200
    /// lines of output and reach Completion concurrently. Verifies
    /// the actor architecture sustains realistic shell output across
    /// more than the spec minimum simultaneously without dropping
    /// events or hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn eight_concurrent_actors_under_fast_output_load() {
        let started = std::time::Instant::now();
        let script = r#"i=1; while [ $i -le 200 ]; do printf 'line %d\n' $i; i=$((i+1)); done; exit 0"#;
        let mut joins = Vec::new();
        for _ in 0..8 {
            let h = TerminalActor::spawn_local(shell_spec(script)).expect("spawn");
            let TerminalHandle {
                mut events_rx,
                command_tx,
                ..
            } = h;
            joins.push(tokio::spawn(async move {
                let _keep = command_tx;
                let evs = collect_until(
                    &mut events_rx,
                    |e| matches!(
                        e.event,
                        TerminalEvent::NeedsAttention {
                            payload: NeedsAttentionPayload {
                                kind: AttentionKind::Completion { .. },
                                ..
                            },
                        }
                    ),
                    5000,
                )
                .await;
                let has_output = evs.iter().any(|e| matches!(e.event, TerminalEvent::Output { .. }));
                let has_exit = evs.iter().any(|e| matches!(e.event, TerminalEvent::Exit { .. }));
                let has_completion = evs.iter().any(|e| matches!(
                    e.event,
                    TerminalEvent::NeedsAttention {
                        payload: NeedsAttentionPayload {
                            kind: AttentionKind::Completion { exit_code: 0 },
                            ..
                        },
                    }
                ));
                (has_output, has_exit, has_completion)
            }));
        }
        for j in joins {
            let (output, exit, completion) = j.await.unwrap();
            assert!(output, "stress actor missed Output");
            assert!(exit, "stress actor missed Exit");
            assert!(completion, "stress actor missed Completion");
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "8-concurrent-PTY stress test must finish under 5s; took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn four_concurrent_actors_all_emit_completion() {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let h = TerminalActor::spawn_local(shell_spec("sleep 0.1; exit 0")).expect("spawn");
            handles.push(h);
        }
        // Run them in parallel; each must observe a Completion event.
        let mut joins = Vec::new();
        for h in handles {
            let TerminalHandle {
                mut events_rx,
                command_tx,
                ..
            } = h;
            joins.push(tokio::spawn(async move {
                let _keep = command_tx;
                let evs = collect_until(
                    &mut events_rx,
                    |e| matches!(
                        e.event,
                        TerminalEvent::NeedsAttention {
                            payload: NeedsAttentionPayload {
                                kind: AttentionKind::Completion { .. },
                                ..
                            },
                        }
                    ),
                    5000,
                )
                .await;
                evs.into_iter().any(|e| matches!(
                    e.event,
                    TerminalEvent::NeedsAttention {
                        payload: NeedsAttentionPayload {
                            kind: AttentionKind::Completion { exit_code: 0 },
                            ..
                        },
                    }
                ))
            }));
        }
        for j in joins {
            assert!(j.await.unwrap(), "concurrent actor missed its Completion");
        }
    }

    /// A test-only transport whose `spawn` always returns a known error.
    /// Proves the actor surfaces the injected transport error without
    /// silently falling back to `LocalTransport`.
    struct FailingTransport {
        spawn_calls: Arc<AtomicUsize>,
    }

    impl Transport for FailingTransport {
        fn spawn(
            &self,
            _request: TransportSpawnRequest,
        ) -> Result<Box<dyn TransportSession>, TransportError> {
            self.spawn_calls.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::SpawnFailed {
                program: "/test/injected".into(),
                message: "injected-failure".into(),
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_propagates_injected_transport_spawn_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let transport: Arc<dyn Transport> = Arc::new(FailingTransport {
            spawn_calls: calls.clone(),
        });

        let result = TerminalActor::spawn(transport, shell_spec("echo should-not-run"));

        match result {
            Err(ActorError::Pty(msg)) => {
                assert!(
                    msg.contains("injected-failure"),
                    "expected injected error text to survive into ActorError, got {msg:?}"
                );
                assert!(
                    msg.contains("/test/injected"),
                    "expected injected program path to survive into ActorError, got {msg:?}"
                );
            }
            Err(other) => panic!("expected ActorError::Pty(...), got {other:?}"),
            Ok(_) => panic!("expected Err, got Ok(TerminalHandle) — local fallback occurred"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "injected transport spawn must be called exactly once (no local fallback)"
        );
    }

    /// Cross-crate proof that an OSC 1339 sequence emitted by the
    /// child PTY surfaces as `AttentionKind::TaskComplete` on the
    /// actor's event channel. Closes the TaskComplete positive path
    /// end-to-end (parser + actor + attention envelope).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn osc_1339_task_complete_surfaces_as_task_complete_attention() {
        // `shipped` base64 = c2hpcHBlZA==
        let script = r#"printf '\033]1339;am-task-complete;success;c2hpcHBlZA==\007'; exit 0"#;
        let h = TerminalActor::spawn_local(shell_spec(script)).expect("spawn");
        let TerminalHandle {
            mut events_rx,
            command_tx,
            ..
        } = h;
        let _keep = command_tx;

        let evs = collect_until(
            &mut events_rx,
            |e| matches!(
                e.event,
                TerminalEvent::NeedsAttention {
                    payload: NeedsAttentionPayload {
                        kind: AttentionKind::TaskComplete { .. },
                        ..
                    },
                }
            ),
            5000,
        )
        .await;

        let task_complete = evs.iter().find_map(|e| match &e.event {
            TerminalEvent::NeedsAttention {
                payload:
                    NeedsAttentionPayload {
                        kind: AttentionKind::TaskComplete { summary },
                        ..
                    },
            } => Some(summary.clone()),
            _ => None,
        });
        assert_eq!(
            task_complete.as_deref(),
            Some("shipped"),
            "child OSC 1339 must produce AttentionKind::TaskComplete with decoded summary; got {evs:?}"
        );
    }

    /// A test-only transport whose session's `wait()` returns
    /// `Disconnect(...)`. Proves the actor's wait-thread → processor
    /// → event-sender pipeline carries the typed Disconnect outcome
    /// and emits `NeedsAttention(AttentionKind::Disconnect)` after
    /// the final `TerminalEvent::Exit { code: None }`.
    struct DisconnectingTransport;

    struct DisconnectingSession {
        output: Option<Box<dyn TransportOutputStream>>,
        sink: Option<Box<dyn TransportStdinSink>>,
        shutdown_handle: Option<Box<dyn TransportShutdownHandle>>,
        resize_handle: Option<Box<dyn TransportResizeHandle>>,
        waited: bool,
    }

    struct NoopShutdownHandle;
    impl TransportShutdownHandle for NoopShutdownHandle {
        fn shutdown(&self, _mode: ShutdownMode) -> Result<(), TransportError> {
            Ok(())
        }
    }

    struct NoopResizeHandle;
    impl TransportResizeHandle for NoopResizeHandle {
        fn resize(&self, _size: crate::transport::PtySize) -> Result<(), TransportError> {
            Ok(())
        }
    }

    struct EmptyReader;
    impl TransportOutputStream for EmptyReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            // Block-then-EOF: sleep briefly so the actor's reader
            // thread isn't a hot loop, then return EOF so the
            // reader_done channel resolves.
            std::thread::sleep(Duration::from_millis(20));
            Ok(0)
        }
    }

    struct NoopSink;
    impl TransportStdinSink for NoopSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Transport for DisconnectingTransport {
        fn spawn(
            &self,
            _request: TransportSpawnRequest,
        ) -> Result<Box<dyn TransportSession>, TransportError> {
            Ok(Box::new(DisconnectingSession {
                output: Some(Box::new(EmptyReader)),
                sink: Some(Box::new(NoopSink)),
                shutdown_handle: Some(Box::new(NoopShutdownHandle)),
                resize_handle: Some(Box::new(NoopResizeHandle)),
                waited: false,
            }))
        }
    }

    impl TransportSession for DisconnectingSession {
        fn output_stream(&mut self) -> Result<Box<dyn TransportOutputStream>, TransportError> {
            self.output.take().ok_or(TransportError::Protocol {
                message: "output already taken".into(),
            })
        }
        fn stdin_sink(&mut self) -> Result<Box<dyn TransportStdinSink>, TransportError> {
            self.sink.take().ok_or(TransportError::Protocol {
                message: "sink already taken".into(),
            })
        }
        fn take_shutdown_handle(&mut self) -> Option<Box<dyn TransportShutdownHandle>> {
            self.shutdown_handle.take()
        }
        fn take_resize_handle(&mut self) -> Option<Box<dyn TransportResizeHandle>> {
            self.resize_handle.take()
        }
        fn resize(&mut self, _size: crate::transport::PtySize) -> Result<(), TransportError> {
            Ok(())
        }
        fn shutdown(&mut self, _mode: ShutdownMode) -> Result<(), TransportError> {
            Ok(())
        }
        fn wait(&mut self) -> Result<TransportExitStatus, TransportError> {
            self.waited = true;
            Ok(TransportExitStatus::Disconnect(
                crate::transport::DisconnectReason::Io("simulated link reset".into()),
            ))
        }
        fn disconnect_reason(&self) -> Option<crate::transport::DisconnectReason> {
            None
        }
        fn cleanup(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_emits_disconnect_attention_when_transport_disconnects() {
        let transport: Arc<dyn Transport> = Arc::new(DisconnectingTransport);
        let h = TerminalActor::spawn(transport, shell_spec("ignored"))
            .expect("disconnecting transport spawns successfully");
        let mut events_rx = h.events_rx;
        let _command_tx = h.command_tx;
        let evs = collect_until(
            &mut events_rx,
            |e| matches!(
                &e.event,
                TerminalEvent::NeedsAttention {
                    payload: NeedsAttentionPayload {
                        kind: AttentionKind::Disconnect,
                        ..
                    },
                }
            ),
            5000,
        )
        .await;
        let exit_with_none = evs
            .iter()
            .find(|e| matches!(&e.event, TerminalEvent::Exit { code: None }));
        assert!(
            exit_with_none.is_some(),
            "Disconnect outcome must emit Exit {{ code: None }}; got {evs:?}"
        );
        let attention = evs.iter().find_map(|e| match &e.event {
            TerminalEvent::NeedsAttention {
                payload: NeedsAttentionPayload {
                    kind: AttentionKind::Disconnect,
                    ..
                },
            } => Some(()),
            _ => None,
        });
        assert!(
            attention.is_some(),
            "Disconnect outcome must emit NeedsAttention(Disconnect); got {evs:?}"
        );
    }
}
