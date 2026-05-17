//! Transport abstraction for terminal-mesh sessions.
//!
//! Spec: `docs/specs/transport.md` sections 2, 3, 4.7. Round 1 ships
//! the trait + `LocalTransport` in isolation; task3 (Round 2+) will
//! refactor `TerminalActor::spawn` to consume `Arc<dyn Transport>`.
//! Until then the existing `actor.rs` path remains the live spawner
//! and these types are exercised only via this module's unit tests.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use portable_pty::{
    native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize as PpPtySize,
};
use thiserror::Error;

// =====================================================================
// Spec section 2 — request / status / error types
// =====================================================================

#[derive(Debug, Clone)]
pub enum WorkspaceLocation {
    Local { path: Option<PathBuf> },
    // Remote { ... } variants land with task14/task16.
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
    // Remote { ... } variants land with task14/task16.
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

    // ---- SSH pre-shell errors (Phase A; scaffolded for task16) ----
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

    // ---- Docker (scaffolded for task17) ----
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
    fn resize(&mut self, size: PtySize) -> Result<(), TransportError>;
    fn shutdown(&mut self, mode: ShutdownMode) -> Result<(), TransportError>;
    fn wait(&mut self) -> Result<TransportExitStatus, TransportError>;
    fn disconnect_reason(&self) -> Option<DisconnectReason>;
    fn cleanup(&mut self) -> Result<(), TransportError>;
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
            workspace: _, // Local-only in Round 1; Remote variants land with task14/task16.
            command,
            initial_size,
            env,
            cwd,
        } = request;

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
            killer,
            reader: Some(reader),
            writer: Some(writer),
            disconnect_reason: None,
        }))
    }
}

struct LocalTransportSession {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    disconnect_reason: Option<DisconnectReason>,
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

    fn resize(&mut self, size: PtySize) -> Result<(), TransportError> {
        let master = self.master.as_mut().ok_or(TransportError::ResizeFailed {
            message: "master pty already dropped".into(),
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
        // portable-pty's ChildKiller::kill() sends SIGHUP on Unix; both
        // Graceful and Kill use the same primitive at this layer. The
        // actor's processor task (actor.rs, ESCALATE_TO_SIGKILL_AFTER)
        // is the only place that owns SIGKILL escalation timing.
        self.killer.kill().map_err(|e| TransportError::ShutdownFailed {
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
