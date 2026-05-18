//! Docker-over-SSH transport composing `SshTransport` per
//! `docs/specs/transport.md` §5.1–§5.4.
//!
//! Remote command shape (§5.1):
//!
//! ```text
//! docker exec -it <container> /bin/sh -lc '<wrapper-script>'
//! ```
//!
//! The wrapper script (§5.2) writes its own PID to
//! `/tmp/agentmesh-wrapper-<session-id>.pid` inside the container,
//! installs an EXIT/HUP/INT/TERM trap to remove the PID file, emits
//! the standard SSH `am-shell-started` / `am-exit-status` OSC 1338
//! sentinels, optionally `cd $AM_REMOTE_CWD`, and execs the login
//! shell. The SSH sentinel parser (`transport_ssh::SentinelParser`)
//! handles the OSC stream unchanged — the container's wrapper emits
//! the same protocol the SSH wrapper does.
//!
//! On `shutdown(Kill)` the transport issues a separate `ssh ...
//! docker exec <container> sh -lc 'kill ...'` over the SAME
//! ControlMaster socket (the connection is multiplexed). The
//! container is NEVER stopped, started, created, or removed; v1
//! attaches to existing user-managed containers only (§5.4).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use uuid::Uuid;

use crate::transport::{
    ContainerLocation, DisconnectReason, PtySize, ShutdownMode, Transport, TransportError,
    TransportExitStatus, TransportOutputStream, TransportResizeHandle, TransportSession,
    TransportShutdownHandle, TransportSpawnRequest, TransportStdinSink, WorkspaceLocation,
};
use crate::transport_ssh::{
    classify_phase_a_failure, compose_remote_command, shell_single_quote,
    spawn_ssh_with_wrapper_script, ssh_control_path_for, PhaseAClassifier, SshLocation,
    SSH_OSC_EXIT_STATUS_PREFIX, SSH_OSC_NUMBER, SSH_OSC_SHELL_STARTED,
};

// ---------------------------------------------------------------------------
// DockerLocation — spec §5.1 + §6.1
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerLocation {
    pub container_id: String,
    pub cwd_in_container: Option<String>,
}

impl DockerLocation {
    /// Extract container info from a `WorkspaceLocation::Remote`
    /// whose `container` field is `Some(...)`. Returns `None` for
    /// Local workspaces or Remote workspaces without a container
    /// — those routes belong to `LocalTransport` / `SshTransport`.
    pub fn from_workspace(workspace: &WorkspaceLocation) -> Option<Self> {
        match workspace {
            WorkspaceLocation::Remote {
                container:
                    Some(ContainerLocation {
                        container_id,
                        cwd_in_container,
                    }),
                ..
            } => Some(Self {
                container_id: container_id.clone(),
                cwd_in_container: cwd_in_container.clone(),
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers — spec §5.1 + §5.2
// ---------------------------------------------------------------------------

/// PID-file path inside the container for the wrapper. Bound to a
/// per-session UUID so concurrent sessions against the same
/// container don't collide.
fn wrapper_pid_file_for(session_id: &str) -> String {
    format!("/tmp/agentmesh-wrapper-{session_id}.pid")
}

/// Build the inner shell script that runs INSIDE the container.
/// Writes the wrapper PID, traps EXIT/HUP/INT/TERM for cleanup,
/// emits `am-shell-started`, optionally `cd $AM_REMOTE_CWD`, execs
/// the login shell, and emits `am-exit-status;<n>`. Returns the
/// raw script body (the caller is responsible for shell-quoting it
/// when embedding it in a `docker exec ... sh -lc '...'` argv slot).
fn build_docker_wrapper_script(
    session_id: &str,
    canonical_remote_path: Option<&str>,
    optional_exec: Option<&crate::transport::ShellCommand>,
) -> String {
    let pid_file = wrapper_pid_file_for(session_id);
    let mut script = String::new();
    script.push_str(&format!(
        "pid_file={}\n",
        shell_single_quote(&pid_file),
    ));
    script.push_str("printf '%s\\n' \"$$\" > \"$pid_file\"\n");
    script.push_str("cleanup_pid_file() { rm -f \"$pid_file\"; }\n");
    script.push_str("trap cleanup_pid_file EXIT HUP INT TERM\n");
    script.push_str(&format!(
        "printf '\\033]{osc};{started}\\a'\n",
        osc = SSH_OSC_NUMBER,
        started = SSH_OSC_SHELL_STARTED,
    ));
    script.push_str("status=0\n");
    if let Some(cwd) = canonical_remote_path {
        let escaped = shell_single_quote(cwd);
        script.push_str(&format!(
            "AM_REMOTE_CWD={escaped}\nif ! cd \"$AM_REMOTE_CWD\" 2>/dev/null; then status=$?; fi\n",
        ));
    }
    match optional_exec {
        None => {
            script.push_str("if [ \"$status\" -eq 0 ]; then if [ -n \"$SHELL\" ] && [ -x \"$SHELL\" ]; then \"$SHELL\" -l; status=$?; else /bin/sh -l; status=$?; fi; fi\n");
        }
        Some(cmd) => {
            // Same shell-quote-and-exec pattern as the SSH wrapper.
            let program = shell_single_quote(&cmd.program.display().to_string());
            let mut line = program;
            for a in &cmd.args {
                line.push(' ');
                line.push_str(&shell_single_quote(a));
            }
            script.push_str(&format!(
                "if [ \"$status\" -eq 0 ]; then {line}\nstatus=$?\nfi\n",
            ));
        }
    }
    script.push_str(&format!(
        "printf '\\033]{osc};{prefix}%s\\a' \"$status\"\nexit \"$status\"\n",
        osc = SSH_OSC_NUMBER,
        prefix = SSH_OSC_EXIT_STATUS_PREFIX,
    ));
    script
}

/// Compose the remote command for the SSH endpoint's final argv
/// slot: `docker exec -it <container> /bin/sh -lc '<inner>'`. The
/// inner wrapper writes the wrapper PID, traps for cleanup, emits
/// the OSC 1338 sentinels, optionally `cd` to the container cwd,
/// and executes either the user's login shell (default) or
/// `optional_exec` (used by auto-launch). Container id is
/// shell-quoted; the inner script is single-quoted via the POSIX
/// `'\''` idiom so apostrophes inside the wrapper round-trip
/// safely.
pub fn compose_docker_remote_command(
    container_id: &str,
    session_id: &str,
    canonical_remote_path: Option<&str>,
    optional_exec: Option<&crate::transport::ShellCommand>,
) -> String {
    let inner = build_docker_wrapper_script(session_id, canonical_remote_path, optional_exec);
    format!(
        "docker exec -it {container} /bin/sh -lc {inner_quoted}",
        container = shell_single_quote(container_id),
        inner_quoted = shell_single_quote(&inner),
    )
}

/// Pre-shell-phase classifier for docker. Recognizes docker
/// stderr patterns BEFORE falling through to the SSH classifier so
/// `docker exec` failures surface as typed
/// `TransportError::{DockerContainerMissing, DockerExecFailed}`
/// instead of being mis-classified as `SshShellDidNotStart`.
///
/// Patterns:
///
/// - `No such container` / `Error response from daemon: No such
///   container` → `DockerContainerMissing`.
/// - `docker: command not found` / `executable file not found ...
///   docker` / `not in $PATH` next to `docker` → `DockerExecFailed`
///   (the remote shell could not find the docker binary).
/// - `the input device is not a TTY` → `DockerExecFailed` (docker
///   refused `-it` because no TTY was allocated downstream — rare
///   when the SSH `-tt` arg is preserved, but defended against).
///
/// Any other failure falls through to the standard SSH classifier
/// so auth / connect / host-key failures still surface correctly.
pub fn classify_docker_phase_a_failure(
    container_id: &str,
    shell_started: bool,
    ssh_exit: Option<i32>,
    stderr_tail: &str,
    location: &SshLocation,
) -> TransportError {
    if stderr_tail.contains("No such container") {
        return TransportError::DockerContainerMissing {
            container: container_id.to_string(),
        };
    }
    if stderr_tail.contains("docker: command not found")
        || (stderr_tail.contains("executable file not found")
            && stderr_tail.contains("docker"))
        || (stderr_tail.contains("docker")
            && stderr_tail.contains("not in $PATH"))
    {
        return TransportError::DockerExecFailed {
            container: container_id.to_string(),
            message: stderr_tail.to_string(),
        };
    }
    if stderr_tail.contains("the input device is not a TTY") {
        return TransportError::DockerExecFailed {
            container: container_id.to_string(),
            message: stderr_tail.to_string(),
        };
    }
    classify_phase_a_failure(shell_started, ssh_exit, stderr_tail, location)
}

/// Construct a `PhaseAClassifier` bound to a specific
/// `container_id` so the docker error variants can carry it as
/// their `container` field.
pub fn docker_phase_a_classifier(container_id: String) -> PhaseAClassifier {
    Arc::new(move |shell_started, ssh_exit, stderr_tail, location| {
        classify_docker_phase_a_failure(
            &container_id,
            shell_started,
            ssh_exit,
            stderr_tail,
            location,
        )
    })
}

/// Compose the cleanup command sent over the same ControlMaster on
/// `shutdown(Kill)`. The container is NEVER stopped — this just
/// signals the wrapper process inside the existing container and
/// removes the PID file.
pub fn compose_docker_cleanup_command(container_id: &str, session_id: &str) -> String {
    let pid_file = wrapper_pid_file_for(session_id);
    let inner = format!(
        "kill \"$(cat {pid_quoted})\" 2>/dev/null || true; rm -f {pid_quoted}",
        pid_quoted = shell_single_quote(&pid_file),
    );
    format!(
        "docker exec {container} sh -lc {inner_quoted}",
        container = shell_single_quote(container_id),
        inner_quoted = shell_single_quote(&inner),
    )
}

// ---------------------------------------------------------------------------
// DockerOverSshTransport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DockerOverSshTransport {
    pub ssh_program: PathBuf,
    pub control_dir: PathBuf,
}

impl DockerOverSshTransport {
    pub fn new(ssh_program: PathBuf, control_dir: PathBuf) -> Self {
        Self {
            ssh_program,
            control_dir,
        }
    }

    /// Production constructor mirroring `SshTransport::from_app_data`.
    pub fn from_app_data(
        ssh_program: PathBuf,
        app_data: &Path,
    ) -> Result<Self, TransportError> {
        let control_dir = crate::transport_ssh::init_control_master_dir(app_data)?;
        if let Err(e) = crate::transport_ssh::cleanup_stale_master_sockets(&control_dir) {
            tracing::warn!(
                control_dir = %control_dir.display(),
                error = %e,
                "DockerOverSshTransport::from_app_data: stale-master cleanup failed (continuing)"
            );
        }
        Ok(Self {
            ssh_program,
            control_dir,
        })
    }
}

impl Transport for DockerOverSshTransport {
    fn spawn(
        &self,
        request: TransportSpawnRequest,
    ) -> Result<Box<dyn TransportSession>, TransportError> {
        let ssh_location = SshLocation::from_workspace(&request.workspace).ok_or_else(|| {
            TransportError::Protocol {
                message: "DockerOverSshTransport requires WorkspaceLocation::Remote".into(),
            }
        })?;
        let docker = DockerLocation::from_workspace(&request.workspace).ok_or_else(|| {
            TransportError::Protocol {
                message:
                    "DockerOverSshTransport requires WorkspaceLocation::Remote with a container"
                        .into(),
            }
        })?;
        let session_id = Uuid::new_v4().to_string();
        // Prefer the in-container cwd when the workspace carries
        // one; fall back to the SSH host path for legacy data where
        // the container mount and SSH path are the same. The legacy
        // fallback is regression-tested separately.
        let cwd_for_wrapper = docker
            .cwd_in_container
            .as_deref()
            .or(Some(ssh_location.canonical_remote_path.as_str()));
        // Mirror SshTransport's optional-exec sentinel: an empty
        // `command.program` means "interactive login shell"; anything
        // else means "exec this program/args inside the container."
        let optional_exec = if request.command.program.as_os_str().is_empty() {
            None
        } else {
            Some(&request.command)
        };
        let docker_cmd = compose_docker_remote_command(
            &docker.container_id,
            &session_id,
            cwd_for_wrapper,
            optional_exec,
        );
        let inner_session = spawn_ssh_with_wrapper_script(
            &self.ssh_program,
            &self.control_dir,
            &ssh_location,
            &docker_cmd,
            request.initial_size,
            docker_phase_a_classifier(docker.container_id.clone()),
        )?;
        Ok(Box::new(DockerOverSshTransportSession {
            inner: Some(inner_session),
            ssh_program: self.ssh_program.clone(),
            control_dir: self.control_dir.clone(),
            ssh_location,
            container_id: docker.container_id,
            session_id,
            cleanup_done: Mutex::new(false),
        }))
    }

    /// Reachability probe: dispatch the same docker-over-SSH spawn
    /// path with an explicit `/bin/sh -lc ':'` no-op command, wait
    /// for the remote `docker exec` to exit cleanly, and return
    /// `Ok(())` on completion. The Docker-aware Phase-A classifier
    /// (`No such container` → `DockerContainerMissing`,
    /// `docker: command not found` → `DockerExecFailed`, etc.)
    /// already wraps `spawn`, so probe errors surface as the
    /// matching typed `TransportError` variants.
    fn probe(&self, workspace: WorkspaceLocation) -> Result<(), TransportError> {
        crate::transport::probe_via_spawn(self, workspace)
    }
}

pub struct DockerOverSshTransportSession {
    inner: Option<Box<dyn TransportSession>>,
    ssh_program: PathBuf,
    control_dir: PathBuf,
    ssh_location: SshLocation,
    container_id: String,
    session_id: String,
    cleanup_done: Mutex<bool>,
}

/// Maximum time the cleanup ssh is allowed to run before the
/// watcher thread kills it. The `Transport::shutdown` contract
/// requires the call to return without waiting, so the cleanup
/// runs entirely in the background under this deadline.
const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

/// Fire the docker-exec-kill cleanup ssh in the background and
/// return immediately. A detached watcher thread enforces a
/// 5-second deadline: on timeout it `kill()`s the cleanup process
/// so the watcher itself exits promptly. Successes and failures
/// are logged via `tracing::warn!` — the caller's `shutdown` path
/// does not (and per the spec must not) wait for or surface this.
fn spawn_cleanup_in_background(
    ssh_program: PathBuf,
    control_path: PathBuf,
    user_host: String,
    port: u16,
    cleanup_cmd: String,
    container_id: String,
) {
    let mut cmd = std::process::Command::new(&ssh_program);
    // `--` ends option parsing so the destination (and any
    // future argv slot) cannot be reinterpreted as a `-`-prefixed
    // option. Defense in depth: SshTransport::spawn already
    // validates `user` and `host` fragments via
    // `validate_ssh_destination_fragment`, but the cleanup argv
    // is built independently and must carry the same guard.
    cmd.arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("ControlMaster=auto")
        .arg("-o").arg("ControlPersist=yes")
        .arg("-o").arg(format!("ControlPath={}", control_path.display()))
        .arg("-o").arg("StrictHostKeyChecking=accept-new")
        .arg("-o").arg("ConnectTimeout=10")
        .arg("-p").arg(port.to_string())
        .arg("--")
        .arg(user_host)
        .arg(compose_remote_command(&cleanup_cmd))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                container = %container_id,
                error = %e,
                "docker container cleanup ssh failed to spawn"
            );
            return;
        }
    };
    let container_id_for_watcher = container_id;
    std::thread::spawn(move || {
        watch_cleanup(child, container_id_for_watcher);
    });
}

fn watch_cleanup(mut child: std::process::Child, container_id: String) {
    let start = std::time::Instant::now();
    let poll = Duration::from_millis(100);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let stderr_tail = child
                        .stderr
                        .take()
                        .map(|mut s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            let _ = s.read_to_string(&mut buf);
                            buf.chars().take(512).collect::<String>()
                        })
                        .unwrap_or_default();
                    tracing::warn!(
                        container = %container_id,
                        exit_code = ?status.code(),
                        stderr = %stderr_tail,
                        "docker container cleanup ssh exited non-zero"
                    );
                }
                return;
            }
            Ok(None) => {
                if start.elapsed() >= CLEANUP_DEADLINE {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(
                        container = %container_id,
                        deadline_ms = CLEANUP_DEADLINE.as_millis() as u64,
                        "docker container cleanup ssh exceeded deadline; killed"
                    );
                    return;
                }
                std::thread::sleep(poll);
            }
            Err(e) => {
                tracing::warn!(
                    container = %container_id,
                    error = %e,
                    "docker container cleanup ssh wait failed"
                );
                return;
            }
        }
    }
}

impl DockerOverSshTransportSession {
    /// Fire-and-forget container cleanup. Idempotent on the
    /// `cleanup_done` flag; non-blocking thanks to
    /// `spawn_cleanup_in_background`. Returns `Ok(())` immediately
    /// in all cases — failures are logged by the watcher thread.
    fn run_container_cleanup(&self) {
        let mut done = match self.cleanup_done.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if *done {
            return;
        }
        *done = true;
        drop(done);
        let control_path = ssh_control_path_for(&self.control_dir, &self.ssh_location);
        let cleanup_cmd = compose_docker_cleanup_command(&self.container_id, &self.session_id);
        spawn_cleanup_in_background(
            self.ssh_program.clone(),
            control_path,
            self.ssh_location.user_host(),
            self.ssh_location.port.unwrap_or(22),
            cleanup_cmd,
            self.container_id.clone(),
        );
    }
}

impl TransportSession for DockerOverSshTransportSession {
    fn output_stream(&mut self) -> Result<Box<dyn TransportOutputStream>, TransportError> {
        self.inner
            .as_mut()
            .ok_or(TransportError::Protocol {
                message: "session already torn down".into(),
            })?
            .output_stream()
    }

    fn stdin_sink(&mut self) -> Result<Box<dyn TransportStdinSink>, TransportError> {
        self.inner
            .as_mut()
            .ok_or(TransportError::Protocol {
                message: "session already torn down".into(),
            })?
            .stdin_sink()
    }

    fn take_shutdown_handle(&mut self) -> Option<Box<dyn TransportShutdownHandle>> {
        let inner_handle = self.inner.as_mut()?.take_shutdown_handle()?;
        Some(Box::new(DockerShutdownHandle {
            inner: inner_handle,
            ssh_program: self.ssh_program.clone(),
            control_dir: self.control_dir.clone(),
            ssh_location: self.ssh_location.clone(),
            container_id: self.container_id.clone(),
            session_id: self.session_id.clone(),
            cleanup_done: Mutex::new(false),
        }))
    }

    fn take_resize_handle(&mut self) -> Option<Box<dyn TransportResizeHandle>> {
        self.inner.as_mut()?.take_resize_handle()
    }

    fn resize(&mut self, size: PtySize) -> Result<(), TransportError> {
        self.inner
            .as_mut()
            .ok_or(TransportError::Protocol {
                message: "session already torn down".into(),
            })?
            .resize(size)
    }

    fn shutdown(&mut self, mode: ShutdownMode) -> Result<(), TransportError> {
        let inner_result = self
            .inner
            .as_mut()
            .ok_or(TransportError::Protocol {
                message: "session already torn down".into(),
            })?
            .shutdown(mode.clone());
        // On Kill, fire the container-side cleanup even if the local
        // shutdown errored — the wrapper inside the container needs
        // to be reaped regardless. The cleanup runs in a detached
        // background thread with its own 5s deadline so this branch
        // returns immediately per the `Transport::shutdown` contract.
        if matches!(mode, ShutdownMode::Kill) {
            self.run_container_cleanup();
        }
        inner_result
    }

    fn wait(&mut self) -> Result<TransportExitStatus, TransportError> {
        self.inner
            .as_mut()
            .ok_or(TransportError::Protocol {
                message: "session already torn down".into(),
            })?
            .wait()
    }

    fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.inner.as_ref().and_then(|s| s.disconnect_reason())
    }

    fn cleanup(&mut self) -> Result<(), TransportError> {
        if let Some(inner) = self.inner.as_mut() {
            inner.cleanup()?;
        }
        self.inner = None;
        Ok(())
    }
}

struct DockerShutdownHandle {
    inner: Box<dyn TransportShutdownHandle>,
    ssh_program: PathBuf,
    control_dir: PathBuf,
    ssh_location: SshLocation,
    container_id: String,
    session_id: String,
    cleanup_done: Mutex<bool>,
}

impl DockerShutdownHandle {
    /// Same fire-and-forget cleanup as the session-side helper.
    /// `shutdown()` must not wait per the `Transport` contract; the
    /// detached watcher thread enforces the 5s deadline and logs
    /// failures via `tracing::warn!`.
    fn run_container_cleanup(&self) {
        let mut done = match self.cleanup_done.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if *done {
            return;
        }
        *done = true;
        drop(done);
        let control_path = ssh_control_path_for(&self.control_dir, &self.ssh_location);
        let cleanup_cmd = compose_docker_cleanup_command(&self.container_id, &self.session_id);
        spawn_cleanup_in_background(
            self.ssh_program.clone(),
            control_path,
            self.ssh_location.user_host(),
            self.ssh_location.port.unwrap_or(22),
            cleanup_cmd,
            self.container_id.clone(),
        );
    }
}

impl TransportShutdownHandle for DockerShutdownHandle {
    fn shutdown(&self, mode: ShutdownMode) -> Result<(), TransportError> {
        let inner_result = self.inner.shutdown(mode.clone());
        if matches!(mode, ShutdownMode::Kill) {
            self.run_container_cleanup();
        }
        inner_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_docker_remote_command_starts_with_docker_exec() {
        let cmd = compose_docker_remote_command("my-container", "sess-1", Some("/srv"), None);
        assert!(
            cmd.starts_with("docker exec -it 'my-container' /bin/sh -lc '"),
            "cmd should start with docker exec -it; got {cmd}"
        );
        assert!(cmd.ends_with('\''), "cmd should end with closing quote");
    }

    #[test]
    fn compose_docker_remote_command_contains_session_pid_file() {
        let cmd = compose_docker_remote_command("c", "abc-123", None, None);
        assert!(
            cmd.contains("/tmp/agentmesh-wrapper-abc-123.pid"),
            "PID file path must include session id; got {cmd}"
        );
    }

    #[test]
    fn compose_docker_remote_command_uses_session_id_to_avoid_collision() {
        let a = compose_docker_remote_command("c", "session-A", None, None);
        let b = compose_docker_remote_command("c", "session-B", None, None);
        assert_ne!(a, b, "different session ids must yield different commands");
        assert!(a.contains("session-A"));
        assert!(b.contains("session-B"));
    }

    #[test]
    fn compose_docker_remote_command_contains_both_osc_sentinels() {
        let cmd = compose_docker_remote_command("c", "s", Some("/srv"), None);
        assert!(cmd.contains("am-shell-started"));
        assert!(cmd.contains("am-exit-status;"));
    }

    /// When `optional_exec` is supplied, the docker wrapper must
    /// replace the login-shell branch with a quoted invocation of
    /// the requested program + args (inside the docker exec).
    #[test]
    fn compose_docker_remote_command_with_exec_runs_command_in_container() {
        use crate::transport::ShellCommand;
        let cmd = ShellCommand {
            program: std::path::PathBuf::from("/usr/bin/claude"),
            args: vec!["--dangerously-skip-permissions".into()],
        };
        let composed = compose_docker_remote_command("ctr", "s1", Some("/srv"), Some(&cmd));
        // The composed command is the OUTER `docker exec -it ctr
        // /bin/sh -lc '<inner>'`. The INNER body is single-quoted
        // for shell-safety, which `'\''` -escapes the apostrophes
        // around `/usr/bin/claude`. Assert the structural pieces
        // are present.
        assert!(
            composed.contains("/usr/bin/claude"),
            "composed must reference the claude program; got {composed}"
        );
        assert!(
            composed.contains("--dangerously-skip-permissions"),
            "composed must reference the requested argv; got {composed}"
        );
        // The login-shell fallback `\"$SHELL\" -l` must NOT appear
        // when an exec was supplied. The composed string survives
        // two layers of POSIX single-quoting, so the literal
        // `"$SHELL"` is escaped to `\"$SHELL\"` (one backslash, two
        // double-quotes); match on the bare token to be flexible.
        assert!(
            !composed.contains("$SHELL"),
            "exec mode must not fall through to login shell; got {composed}"
        );
    }

    #[test]
    fn compose_docker_remote_command_escapes_apostrophe_in_remote_cwd() {
        let cmd = compose_docker_remote_command("c", "s", Some("/srv/it's mine"), None);
        // The path embeds inside two layers of single-quoting (outer
        // docker `sh -lc '...'` + inner POSIX escape). Round-trip via
        // /bin/sh -c proves the wrapper still parses correctly.
        // We can't actually invoke docker, but we can at least check
        // that the apostrophe is escaped (no bare `'` left dangling).
        assert!(
            !cmd.contains("/srv/it's mine"),
            "raw apostrophe must NOT survive — should be POSIX-escaped"
        );
        assert!(cmd.contains("/srv/it"), "path prefix should still appear");
    }

    #[test]
    fn compose_docker_cleanup_command_targets_session_pid_file() {
        let cmd = compose_docker_cleanup_command("my-container", "sess-1");
        assert!(cmd.contains("docker exec 'my-container'"));
        assert!(cmd.contains("kill"));
        assert!(cmd.contains("/tmp/agentmesh-wrapper-sess-1.pid"));
        assert!(cmd.contains("rm -f"));
    }

    /// Spec §5.4: NEVER `docker stop`, `docker run`, `docker start`,
    /// `docker rm`, `docker create`. The spawn + cleanup commands
    /// must not contain any of these forbidden subcommands.
    #[test]
    fn neither_spawn_nor_cleanup_emits_forbidden_docker_subcommands() {
        let spawn_cmd = compose_docker_remote_command("c", "s", Some("/srv"), None);
        let cleanup_cmd = compose_docker_cleanup_command("c", "s");
        for forbidden in [
            "docker stop",
            "docker run",
            "docker start",
            "docker rm",
            "docker create",
        ] {
            assert!(
                !spawn_cmd.contains(forbidden),
                "spawn command must not contain `{forbidden}`; got {spawn_cmd}",
            );
            assert!(
                !cleanup_cmd.contains(forbidden),
                "cleanup command must not contain `{forbidden}`; got {cleanup_cmd}",
            );
        }
    }

    #[test]
    fn docker_location_from_workspace_extracts_container() {
        let ws = WorkspaceLocation::Remote {
            user: None,
            host: "h".into(),
            port: None,
            canonical_remote_path: "/srv".into(),
            container: Some(ContainerLocation {
                container_id: "ctr-7".into(),
                cwd_in_container: None,
            }),
        };
        let loc = DockerLocation::from_workspace(&ws).expect("container present");
        assert_eq!(loc.container_id, "ctr-7");
    }

    #[test]
    fn docker_location_from_workspace_returns_none_for_remote_without_container() {
        let ws = WorkspaceLocation::Remote {
            user: None,
            host: "h".into(),
            port: None,
            canonical_remote_path: "/srv".into(),
            container: None,
        };
        assert!(DockerLocation::from_workspace(&ws).is_none());
    }

    /// `cwd_in_container` MUST survive the registry → core →
    /// `compose_docker_remote_command` pipeline so a workspace whose
    /// container mount differs from the SSH host path lands in the
    /// correct directory inside the container.
    #[test]
    fn docker_wrapper_uses_cwd_in_container_when_present() {
        let cmd = compose_docker_remote_command("c", "s", Some("/app/work"), None);
        assert!(
            cmd.contains("/app/work"),
            "compose must use the supplied container cwd; got {cmd}"
        );
    }

    /// Legacy data without a per-container cwd must fall back to the
    /// SSH host path via the spawn-time `or` chain. Pinned here at
    /// the helper layer; the spawn-path fallback is exercised via
    /// the stub integration tests (the recorder catches whichever
    /// cwd the wrapper actually receives).
    #[test]
    fn docker_wrapper_falls_back_when_cwd_in_container_is_none() {
        // Mirror the spawn-path or-chain: cwd_in_container: None
        // becomes the SSH path. Helper-level smoke that the helper
        // accepts None and the spawn site is the one that supplies
        // the fallback.
        let cmd = compose_docker_remote_command("c", "s", None, None);
        assert!(
            !cmd.contains("AM_REMOTE_CWD"),
            "compose with None cwd must omit the cd block; got {cmd}"
        );
    }

    fn loc() -> SshLocation {
        SshLocation {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(2222),
            canonical_remote_path: "/srv".into(),
        }
    }

    #[test]
    fn classify_docker_phase_a_failure_maps_no_such_container_to_typed_docker_error() {
        let err = classify_docker_phase_a_failure(
            "my-container",
            false,
            Some(1),
            "Error response from daemon: No such container: my-container",
            &loc(),
        );
        match err {
            TransportError::DockerContainerMissing { container } => {
                assert_eq!(container, "my-container");
            }
            other => panic!("expected DockerContainerMissing; got {other:?}"),
        }
    }

    #[test]
    fn classify_docker_phase_a_failure_maps_docker_not_found_to_docker_exec_failed() {
        let err = classify_docker_phase_a_failure(
            "c",
            false,
            Some(127),
            "bash: docker: command not found",
            &loc(),
        );
        assert!(matches!(err, TransportError::DockerExecFailed { .. }));
    }

    #[test]
    fn classify_docker_phase_a_failure_maps_not_a_tty_to_docker_exec_failed() {
        let err = classify_docker_phase_a_failure(
            "c",
            false,
            Some(1),
            "the input device is not a TTY",
            &loc(),
        );
        assert!(matches!(err, TransportError::DockerExecFailed { .. }));
    }

    /// Unrelated SSH failures must still classify via the SSH path
    /// (e.g. auth failure should remain `SshAuth`, NOT a Docker
    /// variant, even though the docker classifier ran first).
    #[test]
    fn classify_docker_phase_a_failure_falls_through_to_ssh_classifier_for_unrelated_stderr() {
        let err = classify_docker_phase_a_failure(
            "c",
            false,
            Some(255),
            "alice@h.example: Permission denied (publickey).",
            &loc(),
        );
        match err {
            TransportError::SshAuth { user, host, port } => {
                assert_eq!(user, "alice");
                assert_eq!(host, "h.example");
                assert_eq!(port, 2222);
            }
            other => panic!("expected SshAuth fall-through; got {other:?}"),
        }
    }

    #[test]
    fn docker_location_from_workspace_preserves_cwd_in_container() {
        let ws = WorkspaceLocation::Remote {
            user: None,
            host: "h".into(),
            port: None,
            canonical_remote_path: "/srv".into(),
            container: Some(ContainerLocation {
                container_id: "ctr-9".into(),
                cwd_in_container: Some("/app/inside".into()),
            }),
        };
        let loc = DockerLocation::from_workspace(&ws).expect("present");
        assert_eq!(loc.container_id, "ctr-9");
        assert_eq!(loc.cwd_in_container.as_deref(), Some("/app/inside"));
    }

    /// `Transport::shutdown` must return without waiting (spec §2).
    /// The cleanup dispatcher must therefore return immediately even
    /// when the cleanup ssh hangs. Drive this with a stub-ssh that
    /// sleeps 30 seconds; assert the dispatcher returns sub-second.
    #[cfg(unix)]
    #[test]
    fn cleanup_dispatcher_returns_immediately_even_for_hung_ssh() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let hung_ssh = tmp.path().join("hung-ssh");
        std::fs::write(&hung_ssh, b"#!/bin/sh\nsleep 30\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&hung_ssh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hung_ssh, perms).unwrap();

        let t0 = std::time::Instant::now();
        spawn_cleanup_in_background(
            hung_ssh,
            tmp.path().join("ctrl.sock"),
            "u@h".into(),
            22,
            "echo cleanup".into(),
            "container".into(),
        );
        let elapsed = t0.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "spawn_cleanup_in_background must return immediately; took {elapsed:?}"
        );
    }

    #[test]
    fn docker_location_from_workspace_returns_none_for_local() {
        let ws = WorkspaceLocation::Local { path: None };
        assert!(DockerLocation::from_workspace(&ws).is_none());
    }
}
