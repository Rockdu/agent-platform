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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::dedup_arbiter::{ArbiterDecision, DedupArbiter};
use crate::events::{
    dedup_key, AttentionKind, BufferTruncated, NeedsAttentionPayload, TerminalEvent,
    TerminalEventEnvelope, TERMINAL_MESH_PLUGIN_ID,
};
use crate::osc_agent_marker::OscAgentMarkerParser;
use crate::prompt_detector::PromptDetector;
use crate::ring_buffer::RingBuffer;

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

pub struct TerminalActor;

impl TerminalActor {
    /// Spawn a Terminal Mesh actor with `spec`. The actor runs on the
    /// ambient Tokio runtime; the reader is a standalone `std::thread`
    /// so a blocking PTY read can't starve the runtime.
    pub fn spawn(spec: TerminalSpec) -> Result<TerminalHandle, ActorError> {
        let TerminalSpec {
            terminal_id,
            command,
            args,
            cwd,
            env,
            cols,
            rows,
        } = spec;

        let pty_system = native_pty_system();
        let pty_pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ActorError::Pty(e.to_string()))?;

        let mut cmd_builder = CommandBuilder::new(&command);
        for a in &args {
            cmd_builder.arg(a);
        }
        if let Some(dir) = cwd.as_ref() {
            cmd_builder.cwd(dir);
        }
        for (k, v) in &env {
            cmd_builder.env(k, v);
        }

        let mut child = pty_pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| ActorError::Pty(e.to_string()))?;
        // Clone an independent killer BEFORE moving the child into the
        // wait thread; the killer is non-blocking and lives in the
        // processor task so Shutdown can fire without racing the
        // wait-thread's blocking `child.wait()`.
        let killer: Box<dyn ChildKiller + Send + Sync> = child.clone_killer();
        // Capture the child's PID for SIGKILL escalation; portable-pty
        // 0.9's ProcessSignaller::kill() only sends SIGHUP on Unix, so
        // a child that traps HUP needs an out-of-band hard kill that
        // the processor task issues if `exit_rx` doesn't resolve
        // within the escalation window.
        let child_pid: Option<u32> = child.process_id();
        // Drop the slave half so the child owns it exclusively.
        drop(pty_pair.slave);

        let mut reader = pty_pair
            .master
            .try_clone_reader()
            .map_err(|e| ActorError::Pty(e.to_string()))?;
        let writer = pty_pair
            .master
            .take_writer()
            .map_err(|e| ActorError::Pty(e.to_string()))?;
        let master: Box<dyn MasterPty + Send> = pty_pair.master;

        let (events_tx, events_rx) = mpsc::channel::<TerminalEventEnvelope>(EVENT_CHANNEL_CAPACITY);
        let (status_tx, status_rx) = mpsc::channel::<BufferTruncated>(STATUS_CHANNEL_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel::<ActorCommand>(COMMAND_CHANNEL_CAPACITY);
        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(RAW_BYTES_CHANNEL_CAPACITY);
        let (reader_done_tx, reader_done_rx) = oneshot::channel::<()>();
        let (exit_tx, exit_rx) = oneshot::channel::<Option<i32>>();

        // Reader thread (sync). Reads from `reader` until EOF or error;
        // forwards every chunk over `bytes_tx`. Signals `reader_done_tx`
        // when finished so the processor can drain pending bytes.
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

        // Exit waiter — `Child::wait` is blocking; offload to a thread
        // that owns the child outright. Killing happens through the
        // pre-cloned `killer` in the processor task, so we don't need
        // any Mutex/Arc indirection here.
        std::thread::Builder::new()
            .name(format!("terminal-mesh-wait-{terminal_id}"))
            .spawn(move || {
                let code = child
                    .wait()
                    .ok()
                    .map(|status| status.exit_code() as i32);
                let _ = exit_tx.send(code);
            })
            .map_err(|e| ActorError::Io(e.to_string()))?;

        // Processor + command listener — single tokio task because they
        // share state (ring buffer, parser, master PTY for resize).
        tokio::spawn(processor_task(
            terminal_id,
            master,
            writer,
            killer,
            child_pid,
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
}

#[allow(clippy::too_many_arguments)]
async fn processor_task(
    terminal_id: Uuid,
    master: Box<dyn MasterPty + Send>,
    mut writer: Box<dyn std::io::Write + Send>,
    mut killer: Box<dyn ChildKiller + Send + Sync>,
    child_pid: Option<u32>,
    mut bytes_rx: mpsc::Receiver<Vec<u8>>,
    mut command_rx: mpsc::Receiver<ActorCommand>,
    events_tx: mpsc::Sender<TerminalEventEnvelope>,
    status_tx: mpsc::Sender<BufferTruncated>,
    mut reader_done_rx: oneshot::Receiver<()>,
    mut exit_rx: oneshot::Receiver<Option<i32>>,
) {
    let mut ring = RingBuffer::new();
    let mut osc = OscAgentMarkerParser::new();
    let mut prompt = PromptDetector::default_bash_zsh();
    let mut arbiter = DedupArbiter::for_production();
    let mut tick = tokio::time::interval(PROMPT_TICK_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown_requested = false;
    let mut child_exit_code: Option<Option<i32>> = None;
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
            code = &mut exit_rx, if !exit_observed => {
                child_exit_code = Some(code.unwrap_or(None));
                exit_observed = true;
                // Drop the writer so the master fd refcount goes down,
                // helping the kernel propagate EOF to the reader.
                drop(std::mem::replace(
                    &mut writer,
                    Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>,
                ));
                drain_deadline = Some(
                    tokio::time::Instant::now() + Duration::from_millis(250),
                );
            }

            // ---- commands from the caller ----
            cmd = command_rx.recv(), if !commands_closed => {
                match cmd {
                    Some(ActorCommand::WriteStdin(bytes)) => {
                        if let Err(e) = writer.write_all(&bytes) {
                            tracing::warn!(%terminal_id, %e, "stdin write failed");
                        }
                        let _ = writer.flush();
                    }
                    Some(ActorCommand::Resize { cols, rows }) => {
                        if let Err(e) = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        }) {
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
                        // Step 1 (graceful): SIGHUP via the cloned
                        // killer (portable-pty 0.9's ProcessSignaller
                        // on Unix). A well-behaved child unwinds and
                        // exits; the wait thread then resolves
                        // `exit_rx`.
                        if let Err(e) = killer.kill() {
                            tracing::warn!(%terminal_id, %e, "child kill (SIGHUP) failed");
                        }
                        // Step 2 (escalation): if exit_rx hasn't
                        // resolved within the grace window, the new
                        // select arm below sends SIGKILL out-of-band.
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
                            emit_attention(
                                &mut arbiter,
                                terminal_id,
                                &events_tx,
                                AttentionKind::AgentMarker {
                                    summary: ev.summary,
                                    severity: ev.severity,
                                },
                            )
                            .await;
                        }
                        let tail = ring.read_scrollback(PROMPT_TAIL_BYTES);
                        if prompt.poll(&tail, Instant::now()).is_some() {
                            emit_attention(
                                &mut arbiter,
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
                        &mut arbiter,
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
                send_hard_kill(terminal_id, child_pid, &mut killer);
                // Single-shot: clear the deadline so the arm doesn't
                // fire repeatedly. If SIGKILL itself somehow fails to
                // terminate the child, the wait thread stays blocked
                // and the loop falls through to the drain-deadline
                // exit path on the next iteration anyway.
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

    let code = child_exit_code.flatten();
    let envelope = TerminalEventEnvelope::now(terminal_id, TerminalEvent::Exit { code });
    let _ = events_tx.send(envelope).await;
    // Emit Completion / NonZeroExit through the dedup arbiter.
    if let Some(c) = code {
        let kind = if c == 0 {
            AttentionKind::Completion { exit_code: 0 }
        } else {
            AttentionKind::NonZeroExit { exit_code: c }
        };
        emit_attention(&mut arbiter, terminal_id, &events_tx, kind).await;
    }
    if shutdown_requested {
        let env = TerminalEventEnvelope::now(terminal_id, TerminalEvent::Cancelled);
        let _ = events_tx.send(env).await;
    }
}

/// Hard-kill escalation. On Unix this delivers SIGKILL to both the
/// child PID and (best-effort) its process group — portable-pty
/// `setsid`s the child, so the negative-pid form catches descendants
/// the shell spawned. On non-Unix we re-invoke the cloned killer
/// since portable-pty's Windows path already does `TerminateProcess`.
fn send_hard_kill(
    terminal_id: Uuid,
    child_pid: Option<u32>,
    killer: &mut Box<dyn ChildKiller + Send + Sync>,
) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        if let Some(pid) = child_pid {
            let raw = pid as i32;
            if let Err(e) = kill(Pid::from_raw(raw), Signal::SIGKILL) {
                tracing::warn!(%terminal_id, pid = raw, %e, "SIGKILL escalation to pid failed");
            }
            // Best-effort process-group kill; portable-pty's `setsid`
            // makes the child its own session leader, so this catches
            // any descendants the shell spawned. ESRCH (no such
            // process) is expected if the child already exited
            // between the SIGHUP and SIGKILL — log at debug only.
            if let Err(e) = kill(Pid::from_raw(-raw), Signal::SIGKILL) {
                tracing::debug!(%terminal_id, pgid = -raw, %e, "SIGKILL pgid (best-effort) failed");
            }
        } else {
            tracing::warn!(%terminal_id, "no child_pid captured; falling back to killer.kill()");
            let _ = killer.kill();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child_pid; // not used
        if let Err(e) = killer.kill() {
            tracing::warn!(%terminal_id, %e, "fallback killer.kill() failed on non-unix");
        }
    }
}

async fn emit_attention(
    arbiter: &mut DedupArbiter,
    terminal_id: Uuid,
    events_tx: &mpsc::Sender<TerminalEventEnvelope>,
    kind: AttentionKind,
) {
    let key = dedup_key(TERMINAL_MESH_PLUGIN_ID, terminal_id, &kind);
    let event_id = Uuid::new_v4();
    match arbiter.try_record(&key, event_id, Instant::now()) {
        ArbiterDecision::Fire => {
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
        ArbiterDecision::Suppress { .. } => {
            // Suppressed; not emitted on the user-facing channel.
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
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
        let handle = TerminalActor::spawn(shell_spec("echo hi; exit 0")).expect("spawn");
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
        let h0 = TerminalActor::spawn(shell_spec("exit 0")).expect("spawn");
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

        let h1 = TerminalActor::spawn(shell_spec("exit 7")).expect("spawn");
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
        let h = TerminalActor::spawn(shell_spec("head -1")).expect("spawn");
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
        let h = TerminalActor::spawn(shell_spec("sleep 0.5")).expect("spawn");
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
        let h = TerminalActor::spawn(TerminalSpec {
            terminal_id: Uuid::new_v4(),
            command: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "echo CWD=$(pwd) MY=$MY_VAR; exit 0".into()],
            cwd: Some(tmp.clone()),
            env: vec![("MY_VAR".into(), "hello".into())],
            cols: 80,
            rows: 24,
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
        let h = TerminalActor::spawn(shell_spec("sleep 30")).expect("spawn");
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
        let h = TerminalActor::spawn(shell_spec("trap '' HUP; sleep 30"))
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn four_concurrent_actors_all_emit_completion() {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let h = TerminalActor::spawn(shell_spec("sleep 0.1; exit 0")).expect("spawn");
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
}
