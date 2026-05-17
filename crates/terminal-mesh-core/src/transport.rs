//! Transport abstraction for terminal-mesh sessions.
//!
//! The `Transport` trait describes how an interactive PTY-like session
//! is created (`spawn`), and `TransportSession` owns the resulting
//! lifecycle: output stream, stdin sink, resize, shutdown, wait, and
//! cleanup. `LocalTransport` is the in-process implementation backed
//! by `portable_pty::native_pty_system()`; remote implementations
//! (SSH, SSH-in-Docker) live in their own crates and plug in through
//! the same trait.
//!
//! Spec: `docs/specs/transport.md`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use portable_pty::{
    native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize as PpPtySize,
};
use thiserror::Error;

// =====================================================================
// Spec section 2 — request / status / error types
// =====================================================================

#[derive(Debug, Clone)]
pub enum WorkspaceLocation {
    Local {
        path: Option<PathBuf>,
    },
    /// SSH-backed workspace (optionally inside a Docker container on
    /// the remote host). Matches the spec §6.1 identity tuple and
    /// mirrors the registry-side `WorkspaceLocation::Remote` shape.
    Remote {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        canonical_remote_path: String,
        container: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone)]
pub enum PathBufOrRemote {
    Local(PathBuf),
    /// Canonical remote path (passed as `AM_REMOTE_CWD` to the SSH
    /// wrapper script). Local transports ignore this variant; only
    /// `SshTransport` / `DockerOverSshTransport` consume it.
    Remote(String),
}

#[derive(Debug)]
pub struct TransportSpawnRequest {
    pub workspace: WorkspaceLocation,
    pub command: ShellCommand,
    pub initial_size: PtySize,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBufOrRemote>,
}

#[derive(Debug, Clone)]
pub enum ShutdownMode {
    Graceful,
    Kill,
}

#[derive(Debug, Clone)]
pub enum DisconnectReason {
    SshAuth,
    SshConnect,
    SshHostKeyChanged,
    SshProcessExitedBeforeShell,
    RemoteCommandFailed,
    Io(String),
    Unknown(String),
}

#[derive(Debug, Clone)]
pub enum TransportExitStatus {
    CleanCompletion,
    NonZeroExit(i32),
    Signaled(i32),
    Disconnect(DisconnectReason),
}

#[derive(Debug, Error)]
pub enum TransportError {
    // ---- LocalTransport / generic ----
    #[error("spawn failed: {program}: {message}")]
    SpawnFailed { program: String, message: String },
    #[error("io error: {message}")]
    Io { message: String },
    #[error("resize failed: {message}")]
    ResizeFailed { message: String },
    #[error("shutdown failed: {message}")]
    ShutdownFailed { message: String },
    #[error("wait failed: {message}")]
    WaitFailed { message: String },

    // ---- SSH pre-shell errors (returned from `Transport::spawn`,
    //      never emitted as an `AttentionKind` event; consumed by the
    //      SSH transport implementation when it lands) ----
    #[allow(dead_code)]
    #[error("ssh binary not found in PATH")]
    SshBinaryNotFound,
    #[allow(dead_code)]
    #[error("ssh auth failed for {user}@{host}:{port}")]
    SshAuth { user: String, host: String, port: u16 },
    #[allow(dead_code)]
    #[error("ssh connect failed: {host}:{port}: {message}")]
    SshConnect {
        host: String,
        port: u16,
        message: String,
    },
    #[allow(dead_code)]
    #[error("ssh host key changed for {host}:{port}: {message}")]
    SshHostKeyChanged {
        host: String,
        port: u16,
        message: String,
    },
    #[allow(dead_code)]
    #[error("ssh shell did not start for {host}:{port} (ssh_exit={ssh_exit:?})")]
    SshShellDidNotStart {
        host: String,
        port: u16,
        ssh_exit: Option<i32>,
        stderr_tail: String,
    },
    #[allow(dead_code)]
    #[error("ssh control path invalid: {}: {message}", path.display())]
    SshControlPathInvalid { path: PathBuf, message: String },
    #[allow(dead_code)]
    #[error("remote path invalid: {path} on {host}:{port}: {message}")]
    RemotePathInvalid {
        path: String,
        host: String,
        port: u16,
        message: String,
    },

    // ---- Docker errors (consumed by the SSH-in-Docker transport
    //      implementation when it lands) ----
    #[allow(dead_code)]
    #[error("docker container missing: {container}")]
    DockerContainerMissing { container: String },
    #[allow(dead_code)]
    #[error("docker exec failed for {container}: {message}")]
    DockerExecFailed { container: String, message: String },
    #[allow(dead_code)]
    #[error("docker cleanup failed for {container}: {message}")]
    DockerCleanupFailed { container: String, message: String },

    #[error("protocol violation: {message}")]
    Protocol { message: String },
}

// =====================================================================
// Spec section 2 — stream traits
// =====================================================================

pub trait TransportOutputStream: Send {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
}

pub trait TransportStdinSink: Send {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>;
    fn flush(&mut self) -> std::io::Result<()>;
}

// =====================================================================
// Spec section 2 — Transport / TransportSession
// =====================================================================

pub trait Transport: Send + Sync {
    fn spawn(
        &self,
        request: TransportSpawnRequest,
    ) -> Result<Box<dyn TransportSession>, TransportError>;
}

pub trait TransportSession: Send {
    fn output_stream(&mut self) -> Result<Box<dyn TransportOutputStream>, TransportError>;
    fn stdin_sink(&mut self) -> Result<Box<dyn TransportStdinSink>, TransportError>;

    // Concurrent-access escape hatches: see docs/specs/transport.md §2.
    // Default `None` for implementations that cannot decompose; callers
    // fall back to the &mut-self methods and accept exclusion with wait.
    fn take_shutdown_handle(&mut self) -> Option<Box<dyn TransportShutdownHandle>> {
        None
    }
    fn take_resize_handle(&mut self) -> Option<Box<dyn TransportResizeHandle>> {
        None
    }

    fn resize(&mut self, size: PtySize) -> Result<(), TransportError>;
    fn shutdown(&mut self, mode: ShutdownMode) -> Result<(), TransportError>;
    fn wait(&mut self) -> Result<TransportExitStatus, TransportError>;
    fn disconnect_reason(&self) -> Option<DisconnectReason>;
    fn cleanup(&mut self) -> Result<(), TransportError>;
}

pub trait TransportShutdownHandle: Send + Sync {
    fn shutdown(&self, mode: ShutdownMode) -> Result<(), TransportError>;
}

pub trait TransportResizeHandle: Send + Sync {
    fn resize(&self, size: PtySize) -> Result<(), TransportError>;
}

// =====================================================================
// Spec section 3 — LocalTransport
// =====================================================================

#[derive(Default)]
pub struct LocalTransport;

impl LocalTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Transport for LocalTransport {
    fn spawn(
        &self,
        request: TransportSpawnRequest,
    ) -> Result<Box<dyn TransportSession>, TransportError> {
        let TransportSpawnRequest {
            workspace,
            command,
            initial_size,
            env,
            cwd,
        } = request;
        // Defensive: callers SHOULD route Remote workspaces to
        // `SshTransport` / `DockerOverSshTransport`. If a Remote
        // request reaches LocalTransport it is a routing bug, not a
        // recoverable runtime condition.
        if matches!(workspace, WorkspaceLocation::Remote { .. }) {
            return Err(TransportError::Protocol {
                message: "LocalTransport cannot spawn a Remote workspace".into(),
            });
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PpPtySize {
                rows: initial_size.rows,
                cols: initial_size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TransportError::SpawnFailed {
                program: command.program.display().to_string(),
                message: format!("openpty: {e}"),
            })?;

        let mut cmd_builder = CommandBuilder::new(&command.program);
        for a in &command.args {
            cmd_builder.arg(a);
        }
        if let Some(PathBufOrRemote::Local(dir)) = cwd.as_ref() {
            cmd_builder.cwd(dir);
        }
        for (k, v) in &env {
            cmd_builder.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd_builder).map_err(|e| {
            TransportError::SpawnFailed {
                program: command.program.display().to_string(),
                message: format!("spawn_command: {e}"),
            }
        })?;

        let killer = child.clone_killer();
        let child_pid = child.process_id();
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|e| TransportError::Io {
            message: format!("try_clone_reader: {e}"),
        })?;
        let writer = pair.master.take_writer().map_err(|e| TransportError::Io {
            message: format!("take_writer: {e}"),
        })?;

        Ok(Box::new(LocalTransportSession {
            master: Some(pair.master),
            child: Some(child),
            killer: Some(killer),
            child_pid,
            reader: Some(reader),
            writer: Some(writer),
            disconnect_reason: None,
        }))
    }
}

struct LocalTransportSession {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    child_pid: Option<u32>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    disconnect_reason: Option<DisconnectReason>,
}

struct LocalShutdownHandle {
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    child_pid: Option<u32>,
}

impl TransportShutdownHandle for LocalShutdownHandle {
    fn shutdown(&self, mode: ShutdownMode) -> Result<(), TransportError> {
        match mode {
            ShutdownMode::Graceful => {
                // portable-pty's `ChildKiller::kill` sends SIGHUP on
                // Unix; well-behaved children unwind and exit.
                let mut killer = self.killer.lock().map_err(|e| {
                    TransportError::ShutdownFailed {
                        message: format!("killer mutex poisoned: {e}"),
                    }
                })?;
                killer.kill().map_err(|e| TransportError::ShutdownFailed {
                    message: e.to_string(),
                })
            }
            ShutdownMode::Kill => self.hard_kill(),
        }
    }
}

impl LocalShutdownHandle {
    #[cfg(unix)]
    fn hard_kill(&self) -> Result<(), TransportError> {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        if let Some(pid) = self.child_pid {
            let raw = pid as i32;
            kill(Pid::from_raw(raw), Signal::SIGKILL).map_err(|e| {
                TransportError::ShutdownFailed {
                    message: format!("SIGKILL pid {raw}: {e}"),
                }
            })?;
            // Best-effort process-group kill; portable-pty `setsid`s the
            // child, so this catches any descendants the shell spawned.
            // ESRCH (no such process) is expected if the child already
            // exited between SIGHUP and SIGKILL.
            let _ = kill(Pid::from_raw(-raw), Signal::SIGKILL);
            Ok(())
        } else {
            // No PID captured; fall back to the cooperative killer
            // (still SIGHUP on Unix, but it's the best we can do).
            let mut killer = self.killer.lock().map_err(|e| TransportError::ShutdownFailed {
                message: format!("killer mutex poisoned: {e}"),
            })?;
            killer.kill().map_err(|e| TransportError::ShutdownFailed {
                message: e.to_string(),
            })
        }
    }

    #[cfg(not(unix))]
    fn hard_kill(&self) -> Result<(), TransportError> {
        // Windows: portable-pty's `kill` already does `TerminateProcess`.
        let mut killer = self.killer.lock().map_err(|e| TransportError::ShutdownFailed {
            message: format!("killer mutex poisoned: {e}"),
        })?;
        killer.kill().map_err(|e| TransportError::ShutdownFailed {
            message: e.to_string(),
        })
    }
}

struct LocalResizeHandle {
    master: Mutex<Box<dyn MasterPty + Send>>,
}

impl TransportResizeHandle for LocalResizeHandle {
    fn resize(&self, size: PtySize) -> Result<(), TransportError> {
        let master = self.master.lock().map_err(|e| TransportError::ResizeFailed {
            message: format!("master mutex poisoned: {e}"),
        })?;
        master
            .resize(PpPtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TransportError::ResizeFailed {
                message: e.to_string(),
            })
    }
}

struct LocalReaderAdapter(Box<dyn Read + Send>);

impl TransportOutputStream for LocalReaderAdapter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

struct LocalWriterAdapter(Box<dyn Write + Send>);

impl TransportStdinSink for LocalWriterAdapter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl TransportSession for LocalTransportSession {
    fn output_stream(&mut self) -> Result<Box<dyn TransportOutputStream>, TransportError> {
        self.reader
            .take()
            .map(|r| Box::new(LocalReaderAdapter(r)) as Box<dyn TransportOutputStream>)
            .ok_or(TransportError::Protocol {
                message: "output_stream already consumed".into(),
            })
    }

    fn stdin_sink(&mut self) -> Result<Box<dyn TransportStdinSink>, TransportError> {
        self.writer
            .take()
            .map(|w| Box::new(LocalWriterAdapter(w)) as Box<dyn TransportStdinSink>)
            .ok_or(TransportError::Protocol {
                message: "stdin_sink already consumed".into(),
            })
    }

    fn take_shutdown_handle(&mut self) -> Option<Box<dyn TransportShutdownHandle>> {
        let pid = self.child_pid;
        self.killer.take().map(|k| {
            Box::new(LocalShutdownHandle {
                killer: Mutex::new(k),
                child_pid: pid,
            }) as Box<dyn TransportShutdownHandle>
        })
    }

    fn take_resize_handle(&mut self) -> Option<Box<dyn TransportResizeHandle>> {
        self.master.take().map(|m| {
            Box::new(LocalResizeHandle {
                master: Mutex::new(m),
            }) as Box<dyn TransportResizeHandle>
        })
    }

    fn resize(&mut self, size: PtySize) -> Result<(), TransportError> {
        let master = self.master.as_mut().ok_or(TransportError::ResizeFailed {
            message: "master pty handle already taken or session torn down".into(),
        })?;
        master
            .resize(PpPtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TransportError::ResizeFailed {
                message: e.to_string(),
            })
    }

    fn shutdown(&mut self, _mode: ShutdownMode) -> Result<(), TransportError> {
        // `ChildKiller::kill` sends SIGHUP on Unix. SIGKILL escalation
        // for HUP-trapping children is performed by the caller after a
        // grace window using the captured child PID.
        let killer = self.killer.as_mut().ok_or(TransportError::ShutdownFailed {
            message: "shutdown handle already taken or session torn down".into(),
        })?;
        killer.kill().map_err(|e| TransportError::ShutdownFailed {
            message: e.to_string(),
        })
    }

    fn wait(&mut self) -> Result<TransportExitStatus, TransportError> {
        let mut child = self.child.take().ok_or(TransportError::WaitFailed {
            message: "child already waited".into(),
        })?;
        match child.wait() {
            Ok(status) => {
                let code = status.exit_code() as i32;
                if status.success() {
                    Ok(TransportExitStatus::CleanCompletion)
                } else {
                    Ok(TransportExitStatus::NonZeroExit(code))
                }
            }
            Err(e) => {
                let reason = DisconnectReason::Io(e.to_string());
                self.disconnect_reason = Some(reason.clone());
                Ok(TransportExitStatus::Disconnect(reason))
            }
        }
    }

    fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_reason.clone()
    }

    fn cleanup(&mut self) -> Result<(), TransportError> {
        self.reader = None;
        self.writer = None;
        self.master = None;
        self.child = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn request(program: &str, args: &[&str]) -> TransportSpawnRequest {
        TransportSpawnRequest {
            workspace: WorkspaceLocation::Local { path: None },
            command: ShellCommand {
                program: PathBuf::from(program),
                args: args.iter().map(|s| (*s).to_string()).collect(),
            },
            initial_size: PtySize { cols: 80, rows: 24 },
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[test]
    fn local_transport_spawn_round_trip_smoke() {
        let mut session = LocalTransport::new()
            .spawn(request("/bin/echo", &["hello-transport"]))
            .expect("spawn");
        let mut reader = session.output_stream().expect("stream");

        let mut buf = [0u8; 4096];
        let mut accum = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => accum.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }
        let s = String::from_utf8_lossy(&accum);
        assert!(s.contains("hello-transport"), "expected echo output, got {s:?}");

        let status = session.wait().expect("wait");
        assert!(
            matches!(status, TransportExitStatus::CleanCompletion),
            "expected CleanCompletion, got {status:?}"
        );
        session.cleanup().expect("cleanup");
    }

    #[test]
    fn local_transport_resize_does_not_panic() {
        let mut session = LocalTransport::new()
            .spawn(request("/bin/sleep", &["2"]))
            .expect("spawn");
        session
            .resize(PtySize { cols: 132, rows: 50 })
            .expect("resize");
        session.shutdown(ShutdownMode::Kill).expect("shutdown");
        let _ = session.wait().expect("wait");
        session.cleanup().expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn local_transport_stdin_round_trip() {
        // Spawn a child that reads one line of stdin and echoes it back
        // with a recognizable prefix. Verifies the full LocalTransport
        // bidirectional path: stdin_sink writes the line, output_stream
        // delivers the transformed bytes, wait returns CleanCompletion.
        let mut session = LocalTransport::new()
            .spawn(request(
                "/bin/sh",
                &["-c", "IFS= read -r line; printf 'stdin:%s\\n' \"$line\""],
            ))
            .expect("spawn");

        let mut writer = session.stdin_sink().expect("sink");
        writer.write(b"hello-stdin\n").expect("write");
        writer.flush().expect("flush");
        drop(writer);

        let mut reader = session.output_stream().expect("stream");
        let mut accum = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    accum.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&accum).contains("stdin:hello-stdin") {
                        break;
                    }
                }
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }

        let s = String::from_utf8_lossy(&accum);
        assert!(
            s.contains("stdin:hello-stdin"),
            "expected stdin echo, got {s:?}"
        );

        let status = session.wait().expect("wait");
        assert!(
            matches!(status, TransportExitStatus::CleanCompletion),
            "expected CleanCompletion, got {status:?}"
        );
        session.cleanup().expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn local_transport_shutdown_kills_long_running_child() {
        let mut session = LocalTransport::new()
            .spawn(request("/bin/sleep", &["30"]))
            .expect("spawn");
        std::thread::sleep(Duration::from_millis(100));
        session.shutdown(ShutdownMode::Kill).expect("shutdown");

        let start = Instant::now();
        let status = session.wait().expect("wait");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "wait did not return promptly after shutdown (elapsed={:?})",
            start.elapsed()
        );
        assert!(
            !matches!(status, TransportExitStatus::CleanCompletion),
            "expected non-clean exit after Kill, got {status:?}"
        );
        session.cleanup().expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn take_handles_decompose_session_for_concurrent_dispatch() {
        // After take_*_handle, the session's own resize/shutdown methods
        // must return typed errors so the ownership transfer is explicit.
        // The extracted handles must successfully dispatch from threads
        // that do not own &mut self on the session.
        use std::sync::Arc;

        let mut session = LocalTransport::new()
            .spawn(request("/bin/sleep", &["30"]))
            .expect("spawn");
        let shutdown: Arc<dyn TransportShutdownHandle> =
            Arc::from(session.take_shutdown_handle().expect("shutdown handle"));
        let resize: Arc<dyn TransportResizeHandle> =
            Arc::from(session.take_resize_handle().expect("resize handle"));

        assert!(matches!(
            session.shutdown(ShutdownMode::Kill),
            Err(TransportError::ShutdownFailed { .. })
        ));
        assert!(matches!(
            session.resize(PtySize { cols: 120, rows: 30 }),
            Err(TransportError::ResizeFailed { .. })
        ));

        resize
            .resize(PtySize { cols: 132, rows: 50 })
            .expect("resize via handle");

        let shutdown_clone: Arc<dyn TransportShutdownHandle> = Arc::clone(&shutdown);
        let waiter = std::thread::spawn(move || session.wait());
        std::thread::sleep(Duration::from_millis(100));
        shutdown_clone
            .shutdown(ShutdownMode::Kill)
            .expect("shutdown via handle");

        let status = waiter.join().expect("waiter join").expect("wait");
        assert!(
            !matches!(status, TransportExitStatus::CleanCompletion),
            "expected non-clean exit after Kill via handle, got {status:?}"
        );
    }

    #[test]
    fn transport_error_variants_compile_and_match_spec() {
        let errs: Vec<TransportError> = vec![
            TransportError::SpawnFailed {
                program: "/bin/false".into(),
                message: "x".into(),
            },
            TransportError::SshAuth {
                user: "u".into(),
                host: "h".into(),
                port: 22,
            },
            TransportError::SshConnect {
                host: "h".into(),
                port: 22,
                message: "refused".into(),
            },
            TransportError::SshHostKeyChanged {
                host: "h".into(),
                port: 22,
                message: "changed".into(),
            },
            TransportError::SshShellDidNotStart {
                host: "h".into(),
                port: 22,
                ssh_exit: Some(255),
                stderr_tail: "tail".into(),
            },
            TransportError::RemotePathInvalid {
                path: "/nope".into(),
                host: "h".into(),
                port: 22,
                message: "ENOENT".into(),
            },
            TransportError::SshControlPathInvalid {
                path: PathBuf::from("/tmp/cm.sock"),
                message: "mode 0666".into(),
            },
            TransportError::DockerContainerMissing {
                container: "c".into(),
            },
            TransportError::Protocol {
                message: "bad sentinel".into(),
            },
        ];
        for err in &errs {
            assert!(!err.to_string().is_empty(), "Display empty for {err:?}");
        }
    }
}
