//! SSH transport implementation backed by the system `ssh` binary.
//!
//! Spec: `docs/specs/transport.md` §4.1–§4.7.
//!
//! Architecture: `SshTransport::spawn` builds the system-ssh argv
//! per §4.1, allocates a local PTY via `portable_pty` so the `-tt`
//! flag works correctly, executes `sh -lc <wrapper>` on the remote
//! side (the wrapper emits OSC 1338 shell-started / exit-status
//! sentinels per §4.5), and gates the pre-shell phase error classification
//! before returning a live `TransportSession`.
//!
//! Pre-shell failures (pre-shell phase) are returned synchronously from
//! `spawn` as typed `TransportError` variants per §4.6 rows 4–8.
//! Post-shell failures (post-shell phase) surface through `wait` /
//! `disconnect_reason` and may emit `AttentionKind` events that DO
//! produce Done-queue entries.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{
    native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize as PpPtySize,
};
use sha2::{Digest, Sha256};

use crate::transport::{
    DisconnectReason, PtySize, ShutdownMode, Transport, TransportError, TransportExitStatus,
    TransportOutputStream, TransportResizeHandle, TransportSession, TransportShutdownHandle,
    TransportSpawnRequest, TransportStdinSink, WorkspaceLocation,
};

// ---------------------------------------------------------------------------
// Constants — spec §4.5
// ---------------------------------------------------------------------------

pub const SSH_OSC_NUMBER: &str = "1338";
pub const SSH_OSC_SHELL_STARTED: &str = "am-shell-started";
pub const SSH_OSC_EXIT_STATUS_PREFIX: &str = "am-exit-status;";
pub const REMOTE_PATH_INVALID_EXIT_CODE: i32 = 77;
const STDERR_TAIL_BYTES: usize = 2 * 1024;
const PHASE_A_DEADLINE: Duration = Duration::from_secs(15);
const CONTROL_PATH_HASH_HEX_LEN: usize = 24;

// ---------------------------------------------------------------------------
// Identity tuple — spec §6.1
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshLocation {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub canonical_remote_path: String,
}

impl SshLocation {
    pub fn from_workspace(workspace: &WorkspaceLocation) -> Option<Self> {
        match workspace {
            WorkspaceLocation::Remote {
                user,
                host,
                port,
                canonical_remote_path,
                container: _,
            } => Some(Self {
                user: user.clone(),
                host: host.clone(),
                port: *port,
                canonical_remote_path: canonical_remote_path.clone(),
            }),
            WorkspaceLocation::Local { .. } => None,
        }
    }

    /// Canonical SSH identity URI used as the hash input for the
    /// ControlPath socket filename. Defaults: port=22, empty user.
    pub fn canonical_identity(&self) -> String {
        let port = self.port.unwrap_or(22);
        match &self.user {
            Some(u) => format!("ssh://{u}@{}:{port}", self.host),
            None => format!("ssh://{}:{port}", self.host),
        }
    }

    pub fn user_host(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// argv builder — spec §4.1
// ---------------------------------------------------------------------------

/// Build the system-ssh argv per spec §4.1. The `wrapper_script` is
/// the body that runs on the remote side; we wrap it via
/// `compose_remote_command` into ONE quoted `sh -lc '...'` string
/// so OpenSSH's "join all post-host argv with spaces and send to the
/// remote shell" rule still produces correct remote semantics.
/// Reject argv-destination fragments (the `user` and `host`
/// strings that build into ssh's positional destination) when
/// they could be reinterpreted as options. OpenSSH parses
/// `-`-prefixed argv as options (e.g. `-oProxyCommand=…`,
/// `-l`); a workspace record with `host` or `user` starting
/// with `-` would let SSH run a local proxy command at probe
/// or spawn time. Reject up front; the argv builder ALSO emits
/// a `--` end-of-options marker as defense-in-depth (see
/// `build_ssh_argv`).
pub fn validate_ssh_destination_fragment(
    value: &str,
    field_label: &'static str,
) -> Result<(), TransportError> {
    if value.is_empty() {
        return Err(TransportError::Protocol {
            message: format!("SSH {field_label} must not be empty"),
        });
    }
    if value.starts_with('-') {
        return Err(TransportError::Protocol {
            message: format!(
                "SSH {field_label} must not start with '-' (option-injection risk): {value:?}"
            ),
        });
    }
    Ok(())
}

pub fn build_ssh_argv(
    control_path: &Path,
    location: &SshLocation,
    wrapper_script: &str,
) -> Result<Vec<String>, TransportError> {
    // Defense-in-depth: reject argv fragments that could be
    // mis-parsed as ssh options BEFORE building argv, even though
    // the `--` separator below also blocks the reinterpretation.
    if let Some(user) = location.user.as_deref() {
        validate_ssh_destination_fragment(user, "user")?;
    }
    validate_ssh_destination_fragment(&location.host, "host")?;
    let port = location.port.unwrap_or(22);
    Ok(vec![
        "-tt".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=yes".into(),
        "-o".into(),
        format!("ControlPath={}", control_path.display()),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-p".into(),
        port.to_string(),
        // `--` ends option parsing so no later argv slot (the
        // destination or the remote command) can be reinterpreted
        // as a `-`-prefixed option even if validation is bypassed
        // by a future caller.
        "--".into(),
        location.user_host(),
        compose_remote_command(wrapper_script),
    ])
}

/// Wrap a multi-line wrapper script in a single shell-safe
/// `sh -lc '...'` invocation. Real OpenSSH joins all argv after the
/// destination with spaces and sends the resulting string to the
/// remote login shell, which then re-parses it. If we passed
/// `["sh", "-lc", <multi-line wrapper>]` as three separate argv
/// elements, the remote shell would parse the joined string and the
/// wrapper's spaces / newlines would split across tokens.
///
/// The escape uses the POSIX `'\''` idiom: close the single-quoted
/// string, append an escaped single quote, then re-open the string.
/// Apostrophes in the wrapper body therefore survive the round-trip
/// through the remote shell's command-line parser.
pub fn compose_remote_command(wrapper_script: &str) -> String {
    format!("sh -lc {}", shell_single_quote(wrapper_script))
}

// ---------------------------------------------------------------------------
// ControlPath hash — spec §4.2
// ---------------------------------------------------------------------------

/// SHA-256 of the canonical SSH identity, encoded as lowercase hex
/// and truncated to `CONTROL_PATH_HASH_HEX_LEN` chars to keep the
/// final socket path under typical Unix socket-path-length limits.
pub fn ssh_control_path_for(control_dir: &Path, location: &SshLocation) -> PathBuf {
    let identity = location.canonical_identity();
    let mut hasher = Sha256::new();
    hasher.update(identity.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex.truncate(CONTROL_PATH_HASH_HEX_LEN);
    control_dir.join(format!("{hex}.sock"))
}

// ---------------------------------------------------------------------------
// Sentinel wrapper script — spec §4.5
// ---------------------------------------------------------------------------

/// Produce the `sh`-compatible wrapper script that runs on the
/// remote side. The script optionally `cd`s into `canonical_remote_path`
/// (failing with exit code 77 if the cd fails, which the host
/// classifier maps to `RemotePathInvalid`), then emits the
/// shell-started sentinel, executes either the user's login shell
/// (default for interactive tabs) or the supplied `optional_exec`
/// command (used by auto-launch so `claude --dangerously-skip-
/// permissions` runs as the remote command), and emits the
/// exit-status sentinel on return.
pub fn build_sentinel_wrapper_script(
    canonical_remote_path: Option<&str>,
    optional_exec: Option<&crate::transport::ShellCommand>,
) -> String {
    let mut script = String::new();
    if let Some(cwd) = canonical_remote_path {
        // Single-quote-escape the path so embedded `'` survives the
        // POSIX shell quoting rules.
        let escaped = shell_single_quote(cwd);
        script.push_str(&format!(
            "AM_REMOTE_CWD={escaped}\nif ! cd \"$AM_REMOTE_CWD\" 2>/dev/null; then exit {code}; fi\n",
            code = REMOTE_PATH_INVALID_EXIT_CODE,
        ));
    }
    script.push_str(&format!(
        "printf '\\033]{osc};{started}\\a'\n",
        osc = SSH_OSC_NUMBER,
        started = SSH_OSC_SHELL_STARTED,
    ));
    match optional_exec {
        None => {
            // Default: interactive login shell.
            script.push_str("if [ -n \"$SHELL\" ] && [ -x \"$SHELL\" ]; then \"$SHELL\" -l; status=$?; else /bin/sh -l; status=$?; fi\n");
        }
        Some(cmd) => {
            // Run a specific command instead of the login shell.
            // Each argv element is single-quoted via the POSIX
            // `'\''` idiom so embedded apostrophes survive intact.
            let program = shell_single_quote(&cmd.program.display().to_string());
            let mut line = program;
            for a in &cmd.args {
                line.push(' ');
                line.push_str(&shell_single_quote(a));
            }
            script.push_str(&line);
            script.push('\n');
            script.push_str("status=$?\n");
        }
    }
    script.push_str(&format!(
        "printf '\\033]{osc};{prefix}%s\\a' \"$status\"\nexit \"$status\"\n",
        osc = SSH_OSC_NUMBER,
        prefix = SSH_OSC_EXIT_STATUS_PREFIX,
    ));
    script
}

pub(crate) fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            // Close the single-quoted string, append an escaped
            // single quote, then re-open the string.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// pre-shell phase classifier — spec §4.6 rows 4–8
// ---------------------------------------------------------------------------

/// Map a pre-shell phase failure (no `am-shell-started` sentinel observed)
/// to the appropriate typed `TransportError`. Pure function: callers
/// supply the captured stderr tail + ssh exit code, the location
/// fields go into the error payload.
pub fn classify_phase_a_failure(
    shell_started: bool,
    ssh_exit: Option<i32>,
    stderr_tail: &str,
    location: &SshLocation,
) -> TransportError {
    assert!(
        !shell_started,
        "classify_phase_a_failure called with shell_started=true; the caller should route through wait()"
    );
    let port = location.port.unwrap_or(22);
    // Host-key change must be checked BEFORE auth — the OpenSSH
    // banner for a changed key often includes the word `password` /
    // `publickey` further down the diagnostic, which would otherwise
    // false-positive as auth failure.
    if stderr_tail.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return TransportError::SshHostKeyChanged {
            host: location.host.clone(),
            port,
            message: stderr_tail.to_string(),
        };
    }
    if stderr_tail.contains("Permission denied")
        && (stderr_tail.contains("publickey") || stderr_tail.contains("password"))
    {
        return TransportError::SshAuth {
            user: location.user.clone().unwrap_or_default(),
            host: location.host.clone(),
            port,
        };
    }
    if stderr_tail.contains("Connection refused")
        || stderr_tail.contains("Could not resolve")
        || stderr_tail.contains("No route to host")
        || stderr_tail.contains("Operation timed out")
        || stderr_tail.contains("Connection timed out")
    {
        return TransportError::SshConnect {
            host: location.host.clone(),
            port,
            message: stderr_tail.to_string(),
        };
    }
    if ssh_exit == Some(REMOTE_PATH_INVALID_EXIT_CODE) {
        return TransportError::RemotePathInvalid {
            path: location.canonical_remote_path.clone(),
            host: location.host.clone(),
            port,
            message: stderr_tail.to_string(),
        };
    }
    TransportError::SshShellDidNotStart {
        host: location.host.clone(),
        port,
        ssh_exit,
        stderr_tail: stderr_tail.to_string(),
    }
}

// ---------------------------------------------------------------------------
// OSC 1338 parser — spec §4.5 + §8.2
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentinelEvent {
    ShellStarted,
    ExitStatus(i32),
}

/// Streaming parser for OSC 1338 sentinel sequences. Maintains a
/// small internal buffer so a payload split across multiple read
/// chunks is reconstructed correctly. Returns the bytes the caller
/// should forward to the terminal (sentinel sequences are stripped).
#[derive(Debug, Default)]
pub struct SentinelParser {
    /// Bytes buffered mid-sequence while we wait for the BEL / ESC
    /// terminator.
    pending: VecDeque<u8>,
    /// `Some(payload-so-far)` while we are inside an OSC 1338 frame.
    in_frame: Option<Vec<u8>>,
}

impl SentinelParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed `bytes` through the parser. Returns the events recognized
    /// + the bytes the caller should pass through to the terminal.
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<SentinelEvent>, Vec<u8>) {
        let mut events = Vec::new();
        let mut output = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if let Some(payload) = self.in_frame.as_mut() {
                // BEL or ESC \ (ST) terminates the OSC frame.
                if b == 0x07 {
                    Self::finish_frame(self.in_frame.take().unwrap(), &mut events);
                    continue;
                }
                if b == 0x1b {
                    // Possible ESC \ (ST). Peek the next byte; if it
                    // is `\\`, end the frame; otherwise treat ESC as
                    // payload (degenerate) and keep accumulating.
                    self.pending.push_back(b);
                    continue;
                }
                if !self.pending.is_empty()
                    && self.pending.front().copied() == Some(0x1b)
                    && b == b'\\'
                {
                    self.pending.pop_front();
                    Self::finish_frame(self.in_frame.take().unwrap(), &mut events);
                    continue;
                }
                payload.push(b);
                continue;
            }
            // Outside a frame: look for the OSC 1338 introducer
            // `ESC ] 1 3 3 8 ;`.
            self.pending.push_back(b);
            if self.try_consume_introducer(&mut output) {
                // Introducer consumed; the parser is now in_frame.
                continue;
            }
            if !self.could_become_introducer() {
                // The buffered bytes cannot complete an OSC 1338
                // introducer; flush them to the output untouched.
                while let Some(buffered) = self.pending.pop_front() {
                    output.push(buffered);
                }
            }
        }
        (events, output)
    }

    fn finish_frame(payload: Vec<u8>, events: &mut Vec<SentinelEvent>) {
        let Ok(s) = std::str::from_utf8(&payload) else {
            // Malformed payload — silently drop per spec.
            return;
        };
        if s == SSH_OSC_SHELL_STARTED {
            events.push(SentinelEvent::ShellStarted);
            return;
        }
        if let Some(rest) = s.strip_prefix(SSH_OSC_EXIT_STATUS_PREFIX)
            && let Ok(n) = rest.parse::<i32>()
        {
            events.push(SentinelEvent::ExitStatus(n));
        }
    }

    fn could_become_introducer(&self) -> bool {
        let needed = b"\x1b]1338;";
        let buffered: Vec<u8> = self.pending.iter().copied().collect();
        if buffered.len() > needed.len() {
            return false;
        }
        needed.starts_with(&buffered)
    }

    fn try_consume_introducer(&mut self, output: &mut Vec<u8>) -> bool {
        let needed = b"\x1b]1338;";
        let buffered: Vec<u8> = self.pending.iter().copied().collect();
        if buffered.len() < needed.len() {
            return false;
        }
        if buffered.starts_with(needed) {
            self.pending.clear();
            self.in_frame = Some(Vec::new());
            return true;
        }
        // Buffered bytes do not start with the SSH OSC introducer.
        // It may still START with an OSC 1337 (TaskComplete) prefix
        // that another parser owns — pass those through untouched.
        while let Some(buffered) = self.pending.pop_front() {
            output.push(buffered);
        }
        false
    }
}

// ---------------------------------------------------------------------------
// ControlMaster directory init + stale cleanup — spec §4.2
// ---------------------------------------------------------------------------

/// Create `${app_data}/ssh-cm/` with mode `0700` and verify it is
/// owned by the current user. Returns the directory path.
pub fn init_control_master_dir(app_data: &Path) -> Result<PathBuf, TransportError> {
    let dir = app_data.join("ssh-cm");
    std::fs::create_dir_all(&dir).map_err(|e| TransportError::SshControlPathInvalid {
        path: dir.clone(),
        message: format!("create_dir_all: {e}"),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(&dir).map_err(|e| TransportError::SshControlPathInvalid {
            path: dir.clone(),
            message: format!("metadata: {e}"),
        })?;
        if meta.uid() != nix::unistd::Uid::current().as_raw() {
            return Err(TransportError::SshControlPathInvalid {
                path: dir.clone(),
                message: format!("directory not owned by current user (uid={})", meta.uid()),
            });
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o700 {
            // Try to tighten; if that fails, reject.
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            if std::fs::set_permissions(&dir, perms).is_err() {
                return Err(TransportError::SshControlPathInvalid {
                    path: dir.clone(),
                    message: format!("directory mode {mode:o} != 0700 and chmod failed"),
                });
            }
        }
    }
    Ok(dir)
}

/// Best-effort stale-socket cleanup. Enumerates `*.sock` files in
/// `control_dir`, removes any whose mtime is older than 1 hour. The
/// 1-hour threshold is conservative — live `ControlPersist` sockets
/// are continuously touched while connections multiplex through
/// them, while sockets left over from crashed sessions are not.
/// Returns the number of sockets removed. Never touches files
/// outside `control_dir`.
pub fn cleanup_stale_master_sockets(control_dir: &Path) -> Result<usize, TransportError> {
    if !control_dir.is_dir() {
        return Ok(0);
    }
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(3600);
    let mut removed = 0usize;
    let entries =
        std::fs::read_dir(control_dir).map_err(|e| TransportError::SshControlPathInvalid {
            path: control_dir.to_path_buf(),
            message: format!("read_dir: {e}"),
        })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sock") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// SshTransport — live implementation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SshTransport {
    pub ssh_program: PathBuf,
    pub control_dir: PathBuf,
}

impl SshTransport {
    /// Construct an `SshTransport` against a pre-validated control
    /// directory. **For tests only**: production callers should use
    /// `from_app_data` so the ControlMaster directory is created,
    /// validated (0700 + Uid::current ownership), and freed of any
    /// stale sockets in one place.
    pub fn new(ssh_program: PathBuf, control_dir: PathBuf) -> Self {
        Self {
            ssh_program,
            control_dir,
        }
    }

    /// Production constructor: creates `${app_data}/ssh-cm/` with
    /// mode 0700, verifies it is owned by the current user, runs
    /// stale-master cleanup once, and stores the validated dir.
    /// Returns `TransportError::SshControlPathInvalid` when the dir
    /// cannot be safely used (unsafe permissions, foreign owner,
    /// permission-denied on cleanup, etc.).
    pub fn from_app_data(
        ssh_program: PathBuf,
        app_data: &Path,
    ) -> Result<Self, TransportError> {
        let control_dir = init_control_master_dir(app_data)?;
        // Best-effort: cleanup failures do NOT fail construction
        // (a non-removable stale socket should not block remote
        // workspace creation), but they are logged.
        if let Err(e) = cleanup_stale_master_sockets(&control_dir) {
            tracing::warn!(
                control_dir = %control_dir.display(),
                error = %e,
                "SshTransport::from_app_data: stale-master cleanup failed (continuing)"
            );
        }
        Ok(Self {
            ssh_program,
            control_dir,
        })
    }
}

/// Verify the existing socket file at `path` is owned by the current
/// user. Returns `Ok(())` when the path is missing (common case;
/// ssh will create the socket itself) or when the owner matches.
/// Returns `SshControlPathInvalid` on a foreign-owned existing file.
#[cfg(unix)]
fn ensure_control_socket_owner_safe(path: &Path) -> Result<(), TransportError> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(TransportError::SshControlPathInvalid {
                path: path.to_path_buf(),
                message: format!("metadata: {e}"),
            });
        }
    };
    let me = nix::unistd::Uid::current().as_raw();
    if meta.uid() != me {
        return Err(TransportError::SshControlPathInvalid {
            path: path.to_path_buf(),
            message: format!(
                "existing ControlPath socket owned by uid={} (current uid={me})",
                meta.uid()
            ),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_control_socket_owner_safe(_path: &Path) -> Result<(), TransportError> {
    Ok(())
}

impl Transport for SshTransport {
    fn spawn(
        &self,
        request: TransportSpawnRequest,
    ) -> Result<Box<dyn TransportSession>, TransportError> {
        let location = SshLocation::from_workspace(&request.workspace).ok_or_else(|| {
            TransportError::Protocol {
                message: "SshTransport requires WorkspaceLocation::Remote".into(),
            }
        })?;
        // An empty `command.program` is the sentinel for "no remote
        // command requested; run an interactive login shell." Any
        // non-empty program means the auto-launch executor (or any
        // future caller) wants the remote side to exec that program
        // in place of the login shell.
        let optional_exec = if request.command.program.as_os_str().is_empty() {
            None
        } else {
            Some(&request.command)
        };
        let wrapper = build_sentinel_wrapper_script(
            Some(&location.canonical_remote_path),
            optional_exec,
        );
        spawn_ssh_with_wrapper_script(
            &self.ssh_program,
            &self.control_dir,
            &location,
            &wrapper,
            request.initial_size,
            default_phase_a_classifier(),
        )
    }

    /// Reachability probe: dispatch the same SSH spawn path with an
    /// explicit `/bin/sh -lc ':'` no-op command, wait for the
    /// remote side to exit cleanly, and return `Ok(())` on
    /// completion. Auth / connect / host-key / exit-77 remote-path
    /// errors propagate as the same typed `TransportError` variants
    /// the spawn path produces, so the registration command can
    /// reuse the existing error-to-DTO mapping.
    fn probe(&self, workspace: WorkspaceLocation) -> Result<(), TransportError> {
        crate::transport::probe_via_spawn(self, workspace)
    }
}

/// Default `PhaseAClassifier` that simply delegates to
/// `classify_phase_a_failure`. Used by `SshTransport`; composed-on
/// by `DockerOverSshTransport`.
pub fn default_phase_a_classifier() -> PhaseAClassifier {
    Arc::new(|shell_started, ssh_exit, stderr_tail, location| {
        classify_phase_a_failure(shell_started, ssh_exit, stderr_tail, location)
    })
}

/// Shared spawn engine extracted from `SshTransport::spawn` so
/// `DockerOverSshTransport` can compose against it without
/// duplicating the local-PTY allocation + sentinel parser thread +
/// pre-shell phase gate. The wrapper script is composed by the
/// caller; this helper builds the argv, spawns ssh under a PTY,
/// runs the gate, and returns either a live session or a typed
/// pre-shell phase error.
/// Pre-shell-phase classifier callback. Receives the captured
/// stderr/output tail + ssh exit code + the SSH location and
/// produces a typed `TransportError`. SshTransport passes a
/// closure that delegates to `classify_phase_a_failure`;
/// DockerOverSshTransport passes a docker-augmented closure that
/// recognizes `No such container` / `docker: command not found` /
/// `not a TTY` patterns BEFORE falling through to the SSH
/// classifier. The pluggable seam is the only way to keep the
/// pre-shell-phase classification a per-transport concern instead
/// of baking SSH-only assumptions into the shared spawn engine.
pub type PhaseAClassifier =
    Arc<dyn Fn(bool, Option<i32>, &str, &SshLocation) -> TransportError + Send + Sync>;

pub(crate) fn spawn_ssh_with_wrapper_script(
    ssh_program: &Path,
    control_dir: &Path,
    location: &SshLocation,
    wrapper_script: &str,
    initial_size: PtySize,
    classifier: PhaseAClassifier,
) -> Result<Box<dyn TransportSession>, TransportError> {
    let control_path = ssh_control_path_for(control_dir, location);
    // Refuse to hand a foreign-owned existing socket file to
    // OpenSSH — that would let another user MITM the SSH session
    // through their ControlMaster. The common case (socket missing)
    // is a no-op.
    ensure_control_socket_owner_safe(&control_path)?;
    let argv = build_ssh_argv(&control_path, location, wrapper_script)?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PpPtySize {
            rows: initial_size.rows,
            cols: initial_size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| TransportError::SpawnFailed {
            program: ssh_program.display().to_string(),
            message: format!("openpty: {e}"),
        })?;

    // portable_pty does not expose a hook to redirect stderr
    // independently. As a pragmatic workaround for v1 we route
    // stderr through the PTY (it will appear in the terminal
    // output) AND scrape the sentinel parser's output for the
    // known stderr patterns. The parser strips OSC 1338 frames
    // before forwarding; OpenSSH stderr lines remain visible.
    let mut cmd_builder = CommandBuilder::new(ssh_program);
    for a in &argv {
        cmd_builder.arg(a);
    }
    let child = pair.slave.spawn_command(cmd_builder).map_err(|e| {
        TransportError::SpawnFailed {
            program: ssh_program.display().to_string(),
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

    let shared = Arc::new(SharedSshState::new());
    let reader_thread = spawn_reader_thread(reader, Arc::clone(&shared));
    let child = Arc::new(Mutex::new(Some(child)));
    let phase_a = await_phase_a(Arc::clone(&shared), Arc::clone(&child), PHASE_A_DEADLINE);

    match phase_a {
        PhaseAOutcome::ShellStarted => Ok(Box::new(SshTransportSession {
            master: Some(pair.master),
            child,
            killer: Some(killer),
            child_pid,
            writer: Some(writer),
            shared,
            reader_thread: Some(reader_thread),
            disconnect_reason: None,
        })),
        PhaseAOutcome::FailedBeforeShell { ssh_exit } => {
            let tail = shared.stderr_tail();
            let err = classifier(false, ssh_exit, &tail, location);
            // Best-effort clean up the child if it is still alive
            // (deadline case).
            if let Ok(mut guard) = child.lock()
                && let Some(mut c) = guard.take()
            {
                let _ = c.kill();
                let _ = c.wait();
            }
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state + pre-shell phase gating
// ---------------------------------------------------------------------------

struct SharedSshState {
    shell_started: AtomicBool,
    exit_status: AtomicI32,
    has_exit_status: AtomicBool,
    /// Bytes that the parser stripped from the PTY output: these
    /// constitute the visible terminal output AND (since stderr is
    /// merged into the PTY by portable_pty) the stderr lines OpenSSH
    /// emits during the pre-shell phase gate. We cap at `STDERR_TAIL_BYTES`
    /// for the classifier; the full output stream is delivered to
    /// the caller via `output_stream`.
    output_buf: Mutex<Vec<u8>>,
    tail: Mutex<VecDeque<u8>>,
    /// Set by the reader thread when the source returns EOF or an
    /// I/O error; the pre-shell phase gate uses this to know the channel
    /// closed.
    reader_done: AtomicBool,
}

impl SharedSshState {
    fn new() -> Self {
        Self {
            shell_started: AtomicBool::new(false),
            exit_status: AtomicI32::new(0),
            has_exit_status: AtomicBool::new(false),
            output_buf: Mutex::new(Vec::new()),
            tail: Mutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)),
            reader_done: AtomicBool::new(false),
        }
    }

    fn push_output(&self, bytes: &[u8]) {
        if let Ok(mut buf) = self.output_buf.lock() {
            buf.extend_from_slice(bytes);
        }
        if let Ok(mut tail) = self.tail.lock() {
            for &b in bytes {
                if tail.len() == STDERR_TAIL_BYTES {
                    tail.pop_front();
                }
                tail.push_back(b);
            }
        }
    }

    fn drain_output(&self) -> Vec<u8> {
        let mut buf = self.output_buf.lock().unwrap();
        std::mem::take(&mut *buf)
    }

    fn stderr_tail(&self) -> String {
        let tail = self.tail.lock().unwrap();
        let bytes: Vec<u8> = tail.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn note_shell_started(&self) {
        self.shell_started.store(true, Ordering::SeqCst);
    }

    fn note_exit_status(&self, n: i32) {
        self.exit_status.store(n, Ordering::SeqCst);
        self.has_exit_status.store(true, Ordering::SeqCst);
    }
}

enum PhaseAOutcome {
    ShellStarted,
    FailedBeforeShell { ssh_exit: Option<i32> },
}

/// After observing the ssh child's exit, drain the reader thread
/// while polling for the `am-shell-started` sentinel. Returns
/// `true` if the sentinel was observed (the wrapper emitted both
/// sentinels before the inner command's fast exit closed the
/// PTY, so the pre-shell handshake actually completed), `false`
/// if the drain finished without ever seeing it (genuine
/// pre-shell failure).
///
/// Bounded by `deadline`: the reader exits on master-PTY EOF,
/// which happens at the latest when ssh exits, so under normal
/// operation the loop terminates promptly. The deadline is a
/// safety bound for pathological IO scheduling.
///
/// Extracted as a pure helper so the race-window classification
/// can be tested without a `portable_pty::Child` stub.
fn drain_and_recheck_shell_started(
    shared: &SharedSshState,
    deadline: Duration,
) -> bool {
    let drain_deadline = Instant::now() + deadline;
    while !shared.reader_done.load(Ordering::SeqCst)
        && Instant::now() < drain_deadline
    {
        if shared.shell_started.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    // Last-chance recheck after the reader has finished (or the
    // drain deadline expired). The reader may have stored the
    // sentinel in the final iteration just before setting
    // reader_done.
    shared.shell_started.load(Ordering::SeqCst)
}

fn await_phase_a(
    shared: Arc<SharedSshState>,
    child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
    deadline: Duration,
) -> PhaseAOutcome {
    let start = Instant::now();
    let poll = Duration::from_millis(25);
    loop {
        if shared.shell_started.load(Ordering::SeqCst) {
            return PhaseAOutcome::ShellStarted;
        }
        // Check child exit.
        let mut exited = None;
        if let Ok(mut guard) = child.lock()
            && let Some(c) = guard.as_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            exited = Some(status.exit_code() as i32);
        }
        if let Some(code) = exited {
            // Fast-exit race: `try_wait` can observe ssh's exit
            // BEFORE the reader thread has parsed the
            // `am-shell-started` sentinel that ssh wrote to the
            // PTY just before exiting (especially for the
            // registration probe's `/bin/sh -lc ':'`). The drain
            // helper polls `reader_done` and `shell_started`
            // until either the sentinel is observed or the
            // reader finishes — closing the race window.
            if drain_and_recheck_shell_started(&shared, Duration::from_millis(500)) {
                return PhaseAOutcome::ShellStarted;
            }
            return PhaseAOutcome::FailedBeforeShell {
                ssh_exit: Some(code),
            };
        }
        if shared.reader_done.load(Ordering::SeqCst) && !shared.shell_started.load(Ordering::SeqCst)
        {
            return PhaseAOutcome::FailedBeforeShell { ssh_exit: None };
        }
        if start.elapsed() >= deadline {
            return PhaseAOutcome::FailedBeforeShell { ssh_exit: None };
        }
        thread::sleep(poll);
    }
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<SharedSshState>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut parser = SentinelParser::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    shared.reader_done.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(n) => {
                    let (events, output) = parser.feed(&buf[..n]);
                    for ev in events {
                        match ev {
                            SentinelEvent::ShellStarted => shared.note_shell_started(),
                            SentinelEvent::ExitStatus(code) => shared.note_exit_status(code),
                        }
                    }
                    shared.push_output(&output);
                }
                Err(_) => {
                    shared.reader_done.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// SshTransportSession
// ---------------------------------------------------------------------------

pub struct SshTransportSession {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    child_pid: Option<u32>,
    writer: Option<Box<dyn Write + Send>>,
    shared: Arc<SharedSshState>,
    reader_thread: Option<thread::JoinHandle<()>>,
    disconnect_reason: Option<DisconnectReason>,
}

struct SshOutputAdapter {
    shared: Arc<SharedSshState>,
    /// Per-reader pending tail: bytes drained from the shared buffer
    /// that did not fit in the caller's `buf`. Subsequent `read`
    /// calls serve from here BEFORE re-draining the shared buffer,
    /// so a small consumer buffer never loses bytes when the SSH
    /// reader thread accumulated a larger chunk between reads.
    pending: VecDeque<u8>,
}

impl TransportOutputStream for SshOutputAdapter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // Serve from the pending tail first if it has bytes.
            if !self.pending.is_empty() {
                let n = self.pending.len().min(buf.len());
                for (i, byte) in self.pending.drain(..n).enumerate() {
                    buf[i] = byte;
                }
                return Ok(n);
            }
            // Pending is empty: drain the shared buffer into pending,
            // then serve. The drain returns whatever the SSH reader
            // thread has accumulated since the last call.
            let chunk = self.shared.drain_output();
            if !chunk.is_empty() {
                self.pending.extend(chunk);
                continue;
            }
            // No buffered bytes anywhere; signal EOF only if upstream
            // is done, else park briefly to wait for the reader.
            if self.shared.reader_done.load(Ordering::SeqCst) {
                return Ok(0);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

struct SshStdinAdapter {
    writer: Box<dyn Write + Send>,
}

impl TransportStdinSink for SshStdinAdapter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

struct SshShutdownHandle {
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    child_pid: Option<u32>,
}

impl TransportShutdownHandle for SshShutdownHandle {
    fn shutdown(&self, mode: ShutdownMode) -> Result<(), TransportError> {
        let mut killer = self.killer.lock().map_err(|e| {
            TransportError::ShutdownFailed {
                message: format!("killer mutex poisoned: {e}"),
            }
        })?;
        killer.kill().map_err(|e| TransportError::ShutdownFailed {
            message: e.to_string(),
        })?;
        #[cfg(unix)]
        if matches!(mode, ShutdownMode::Kill)
            && let Some(pid) = self.child_pid
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }
}

struct SshResizeHandle {
    master: Mutex<Box<dyn MasterPty + Send>>,
}

impl TransportResizeHandle for SshResizeHandle {
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

impl TransportSession for SshTransportSession {
    fn output_stream(&mut self) -> Result<Box<dyn TransportOutputStream>, TransportError> {
        Ok(Box::new(SshOutputAdapter {
            shared: Arc::clone(&self.shared),
            pending: VecDeque::new(),
        }))
    }

    fn stdin_sink(&mut self) -> Result<Box<dyn TransportStdinSink>, TransportError> {
        self.writer
            .take()
            .map(|w| Box::new(SshStdinAdapter { writer: w }) as Box<dyn TransportStdinSink>)
            .ok_or(TransportError::Protocol {
                message: "stdin_sink already consumed".into(),
            })
    }

    fn take_shutdown_handle(&mut self) -> Option<Box<dyn TransportShutdownHandle>> {
        let pid = self.child_pid;
        self.killer.take().map(|k| {
            Box::new(SshShutdownHandle {
                killer: Mutex::new(k),
                child_pid: pid,
            }) as Box<dyn TransportShutdownHandle>
        })
    }

    fn take_resize_handle(&mut self) -> Option<Box<dyn TransportResizeHandle>> {
        self.master.take().map(|m| {
            Box::new(SshResizeHandle {
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
        let killer = self.killer.as_mut().ok_or(TransportError::ShutdownFailed {
            message: "shutdown handle already taken or session torn down".into(),
        })?;
        killer.kill().map_err(|e| TransportError::ShutdownFailed {
            message: e.to_string(),
        })
    }

    fn wait(&mut self) -> Result<TransportExitStatus, TransportError> {
        // Block until the ssh subprocess exits.
        let raw = {
            let mut guard = self
                .child
                .lock()
                .map_err(|e| TransportError::WaitFailed {
                    message: format!("child mutex poisoned: {e}"),
                })?;
            let mut child = guard.take().ok_or(TransportError::WaitFailed {
                message: "child already waited".into(),
            })?;
            child.wait().map_err(|e| TransportError::WaitFailed {
                message: e.to_string(),
            })?
        };
        // Synchronize with the reader thread: once the ssh child
        // has exited, its end of the PTY slave is closed and the
        // master reader will see EOF, so the reader thread will
        // exit its read loop. Joining it here guarantees every
        // byte ssh sent — including the final OSC 1338
        // `am-exit-status` sentinel — has been parsed into
        // `shared.has_exit_status` / `shared.exit_status` BEFORE
        // we classify. The previous 50ms sleep was a flakiness
        // heuristic; under CPU load or for fast-exiting sessions
        // the reader could still be parsing when the sleep
        // expired, leaving `has_exit_status=false` and
        // mis-reporting clean exits as `Disconnect`.
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        let shell_started = self.shared.shell_started.load(Ordering::SeqCst);
        let has_exit = self.shared.has_exit_status.load(Ordering::SeqCst);
        let exit_code = self.shared.exit_status.load(Ordering::SeqCst);
        let ssh_exit = raw.exit_code() as i32;
        if shell_started {
            if has_exit {
                if exit_code == 0 {
                    Ok(TransportExitStatus::CleanCompletion)
                } else {
                    Ok(TransportExitStatus::NonZeroExit(exit_code))
                }
            } else {
                let reason = DisconnectReason::Io(format!(
                    "ssh exited (code={ssh_exit}) without remote exit-status sentinel"
                ));
                self.disconnect_reason = Some(reason.clone());
                Ok(TransportExitStatus::Disconnect(reason))
            }
        } else {
            let reason = DisconnectReason::SshProcessExitedBeforeShell;
            self.disconnect_reason = Some(reason.clone());
            Ok(TransportExitStatus::Disconnect(reason))
        }
    }

    fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_reason.clone()
    }

    fn cleanup(&mut self) -> Result<(), TransportError> {
        self.writer = None;
        self.master = None;
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(user: Option<&str>, host: &str, port: Option<u16>, path: &str) -> SshLocation {
        SshLocation {
            user: user.map(|s| s.to_string()),
            host: host.to_string(),
            port,
            canonical_remote_path: path.to_string(),
        }
    }

    #[test]
    fn build_ssh_argv_emits_full_spec_template() {
        let argv = build_ssh_argv(
            Path::new("/tmp/cm/abc.sock"),
            &loc(Some("alice"), "example.com", Some(2222), "/srv"),
            "echo hi",
        )
        .expect("argv builds for a normal user@host");
        assert!(argv.contains(&"-tt".to_string()));
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        assert!(argv.contains(&"ControlPersist=yes".to_string()));
        assert!(argv.contains(&"ControlPath=/tmp/cm/abc.sock".to_string()));
        assert!(argv.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(argv.contains(&"ConnectTimeout=10".to_string()));
        assert!(argv.contains(&"-p".to_string()));
        assert!(argv.contains(&"2222".to_string()));
        assert!(argv.contains(&"alice@example.com".to_string()));
        // The remote command MUST be ONE argv element so OpenSSH's
        // join-with-spaces rule produces correct remote semantics.
        let last = argv.last().expect("argv non-empty");
        assert_eq!(last, "sh -lc 'echo hi'");
    }

    /// `--` MUST appear immediately before the destination so
    /// no later argv slot can be reinterpreted as an option,
    /// even if validation is bypassed by a future caller.
    #[test]
    fn build_ssh_argv_emits_dash_dash_end_of_options_marker() {
        let argv = build_ssh_argv(
            Path::new("/tmp/cm/abc.sock"),
            &loc(Some("alice"), "example.com", Some(2222), "/srv"),
            "echo hi",
        )
        .expect("argv builds");
        let dest_idx = argv
            .iter()
            .position(|a| a == "alice@example.com")
            .expect("destination present");
        assert!(
            dest_idx > 0,
            "destination cannot be the first argv element"
        );
        assert_eq!(
            argv[dest_idx - 1],
            "--",
            "`--` end-of-options marker must precede the destination"
        );
    }

    /// Reject hosts starting with `-` as an OpenSSH option-
    /// injection vector. Same defense applies to `user`.
    #[test]
    fn build_ssh_argv_rejects_option_like_host() {
        let err = build_ssh_argv(
            Path::new("/tmp/cm/abc.sock"),
            &loc(None, "-oProxyCommand=evil", Some(22), "/srv"),
            "echo hi",
        )
        .expect_err("option-like host must be rejected");
        match err {
            TransportError::Protocol { message } => {
                assert!(
                    message.contains("host"),
                    "Protocol error must mention the rejected field; got {message}"
                );
            }
            other => panic!("expected Protocol; got {other:?}"),
        }
    }

    #[test]
    fn build_ssh_argv_rejects_option_like_user() {
        let err = build_ssh_argv(
            Path::new("/tmp/cm/abc.sock"),
            &loc(Some("-lroot"), "h", Some(22), "/srv"),
            "echo hi",
        )
        .expect_err("option-like user must be rejected");
        match err {
            TransportError::Protocol { message } => {
                assert!(message.contains("user"), "got {message}");
            }
            other => panic!("expected Protocol; got {other:?}"),
        }
    }

    #[test]
    fn validate_ssh_destination_fragment_rejects_leading_dash_and_empty() {
        assert!(validate_ssh_destination_fragment("-oProxyCommand=x", "host").is_err());
        assert!(validate_ssh_destination_fragment("-l", "host").is_err());
        assert!(validate_ssh_destination_fragment("-", "host").is_err());
        assert!(validate_ssh_destination_fragment("", "host").is_err());
        assert!(validate_ssh_destination_fragment("h.example", "host").is_ok());
        assert!(validate_ssh_destination_fragment("127.0.0.1", "host").is_ok());
        assert!(validate_ssh_destination_fragment("alice", "user").is_ok());
    }

    #[test]
    fn compose_remote_command_wraps_wrapper_in_sh_lc_quoted() {
        let cmd = compose_remote_command("printf 'hi'\necho done");
        assert_eq!(cmd, "sh -lc 'printf '\\''hi'\\''\necho done'");
    }

    #[test]
    fn compose_remote_command_round_trips_through_sh_parser() {
        // Mirror how the remote shell will see the composed string:
        // join the would-be argv elements with spaces (OpenSSH's
        // behavior) and re-parse via /bin/sh -c. The wrapper script
        // body must reach the remote-side shell unchanged.
        let wrapper = "printf 'hello'\nexit 7";
        let composed = compose_remote_command(wrapper);
        // Mirror OpenSSH: a single argv element is delivered to the
        // remote shell as-is. /bin/sh -c will re-parse it.
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&composed)
            .output()
            .expect("sh -c");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "hello",
            "wrapper printf must survive sh-quoting round-trip"
        );
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn build_ssh_argv_omits_user_when_none() {
        let argv = build_ssh_argv(
            Path::new("/tmp/cm/abc.sock"),
            &loc(None, "h", None, "/srv"),
            "x",
        )
        .expect("argv builds");
        assert!(argv.contains(&"h".to_string()));
        assert!(!argv.iter().any(|a| a.contains('@')));
    }

    #[test]
    fn build_ssh_argv_defaults_port_22_when_none() {
        let argv = build_ssh_argv(
            Path::new("/tmp/cm/abc.sock"),
            &loc(None, "h", None, "/srv"),
            "x",
        )
        .expect("argv builds");
        let dash_p = argv.iter().position(|a| a == "-p").expect("has -p");
        assert_eq!(argv[dash_p + 1], "22");
    }

    #[test]
    fn ssh_control_path_is_stable_for_same_identity() {
        let dir = PathBuf::from("/tmp/cm");
        let a = ssh_control_path_for(&dir, &loc(Some("u"), "h", Some(22), "/p"));
        let b = ssh_control_path_for(&dir, &loc(Some("u"), "h", Some(22), "/different"));
        // Path is NOT in the hash (per spec: ControlMaster reuse is
        // per SSH endpoint, not per workspace).
        assert_eq!(a, b);
    }

    #[test]
    fn ssh_control_path_differs_for_different_endpoint() {
        let dir = PathBuf::from("/tmp/cm");
        let a = ssh_control_path_for(&dir, &loc(Some("u"), "h", Some(22), "/p"));
        let b = ssh_control_path_for(&dir, &loc(Some("u"), "h2", Some(22), "/p"));
        assert_ne!(a, b);
        let c = ssh_control_path_for(&dir, &loc(Some("u"), "h", Some(2222), "/p"));
        assert_ne!(a, c);
    }

    #[test]
    fn ssh_control_path_filename_is_bounded() {
        let dir = PathBuf::from("/tmp/cm");
        let p = ssh_control_path_for(&dir, &loc(Some("u"), "h", Some(22), "/p"));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), CONTROL_PATH_HASH_HEX_LEN + ".sock".len());
    }

    #[test]
    fn build_sentinel_wrapper_script_emits_shell_started_and_exit_sentinels() {
        let script = build_sentinel_wrapper_script(Some("/srv/work"), None);
        assert!(script.contains("am-shell-started"));
        assert!(script.contains("am-exit-status;"));
        assert!(script.contains("1338"));
        assert!(script.contains("'/srv/work'"));
        assert!(script.contains("exit 77"));
    }

    #[test]
    fn build_sentinel_wrapper_script_omits_cwd_when_none() {
        let script = build_sentinel_wrapper_script(None, None);
        assert!(!script.contains("AM_REMOTE_CWD"));
        assert!(script.contains("am-shell-started"));
    }

    #[test]
    fn build_sentinel_wrapper_script_shell_escapes_apostrophe() {
        let script = build_sentinel_wrapper_script(Some("/srv/it's mine"), None);
        assert!(script.contains("'/srv/it'\\''s mine'"));
    }

    /// When `optional_exec` is supplied, the wrapper must replace
    /// the login-shell branch with a quoted invocation of the
    /// requested program + args. The OSC sentinels stay intact.
    #[test]
    fn build_sentinel_wrapper_script_with_exec_replaces_login_shell() {
        use crate::transport::ShellCommand;
        let cmd = ShellCommand {
            program: std::path::PathBuf::from("/usr/bin/claude"),
            args: vec!["--dangerously-skip-permissions".into()],
        };
        let script = build_sentinel_wrapper_script(Some("/srv"), Some(&cmd));
        assert!(
            !script.contains("\"$SHELL\" -l"),
            "exec mode must not fall through to the login shell branch; got {script}"
        );
        assert!(
            script.contains("'/usr/bin/claude' '--dangerously-skip-permissions'"),
            "wrapper must emit the quoted program + args; got {script}"
        );
        assert!(script.contains("am-shell-started"));
        assert!(script.contains("am-exit-status;"));
    }

    /// Round-trip the composed `sh -lc` remote command through
    /// `/bin/sh -c` to mirror OpenSSH parsing; assert the requested
    /// program's exit code surfaces via the exit-status sentinel.
    /// Runs a harmless `/bin/sh -c "exit 7"` so the test stays
    /// self-contained.
    #[cfg(unix)]
    #[test]
    fn compose_remote_command_round_trips_with_custom_exec() {
        use crate::transport::ShellCommand;
        let cmd = ShellCommand {
            program: std::path::PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 7".into()],
        };
        // Omit the cd block so the script runs anywhere.
        let wrapper = build_sentinel_wrapper_script(None, Some(&cmd));
        let composed = compose_remote_command(&wrapper);
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&composed)
            .output()
            .expect("sh -c");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("am-exit-status;7"),
            "wrapper exit-status sentinel must carry the requested command's exit code; got stdout={stdout:?}"
        );
        // The wrapper itself returns the same code via `exit`.
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn classify_phase_a_failure_matches_auth_pattern() {
        let err = classify_phase_a_failure(
            false,
            Some(255),
            "alice@h: Permission denied (publickey).",
            &loc(Some("alice"), "h", Some(22), "/p"),
        );
        match err {
            TransportError::SshAuth { user, host, port } => {
                assert_eq!(user, "alice");
                assert_eq!(host, "h");
                assert_eq!(port, 22);
            }
            other => panic!("expected SshAuth; got {other:?}"),
        }
    }

    #[test]
    fn classify_phase_a_failure_matches_host_key_changed() {
        let err = classify_phase_a_failure(
            false,
            Some(255),
            "@@@@@\nREMOTE HOST IDENTIFICATION HAS CHANGED!\n@@@@@\nPermission denied (publickey).",
            &loc(None, "h", None, "/p"),
        );
        match err {
            TransportError::SshHostKeyChanged { host, port, message } => {
                assert_eq!(host, "h");
                assert_eq!(port, 22);
                assert!(message.contains("REMOTE HOST"));
            }
            other => panic!("expected SshHostKeyChanged; got {other:?}"),
        }
    }

    #[test]
    fn classify_phase_a_failure_matches_connect_refused() {
        let err = classify_phase_a_failure(
            false,
            Some(255),
            "ssh: connect to host h port 22: Connection refused",
            &loc(None, "h", None, "/p"),
        );
        assert!(matches!(err, TransportError::SshConnect { .. }));
    }

    #[test]
    fn classify_phase_a_failure_exit_77_maps_to_remote_path_invalid() {
        let err = classify_phase_a_failure(
            false,
            Some(77),
            "",
            &loc(None, "h", None, "/nope"),
        );
        match err {
            TransportError::RemotePathInvalid { path, host, .. } => {
                assert_eq!(path, "/nope");
                assert_eq!(host, "h");
            }
            other => panic!("expected RemotePathInvalid; got {other:?}"),
        }
    }

    #[test]
    fn classify_phase_a_failure_catchall_unknown_pattern() {
        let err = classify_phase_a_failure(
            false,
            Some(2),
            "something unrelated",
            &loc(None, "h", None, "/p"),
        );
        match err {
            TransportError::SshShellDidNotStart {
                ssh_exit,
                stderr_tail,
                ..
            } => {
                assert_eq!(ssh_exit, Some(2));
                assert_eq!(stderr_tail, "something unrelated");
            }
            other => panic!("expected SshShellDidNotStart; got {other:?}"),
        }
    }

    #[test]
    fn parse_osc_1338_single_shell_started() {
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"\x1b]1338;am-shell-started\x07");
        assert_eq!(events, vec![SentinelEvent::ShellStarted]);
        assert!(output.is_empty());
    }

    #[test]
    fn parse_osc_1338_single_exit_status_42() {
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"\x1b]1338;am-exit-status;42\x07");
        assert_eq!(events, vec![SentinelEvent::ExitStatus(42)]);
        assert!(output.is_empty());
    }

    #[test]
    fn parse_osc_1338_split_across_chunks() {
        let mut p = SentinelParser::new();
        let (e1, o1) = p.feed(b"\x1b]1338;am-shel");
        assert!(e1.is_empty());
        assert!(o1.is_empty());
        let (e2, o2) = p.feed(b"l-started\x07");
        assert_eq!(e2, vec![SentinelEvent::ShellStarted]);
        assert!(o2.is_empty());
    }

    #[test]
    fn parse_osc_1338_multiple_in_one_chunk() {
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"\x1b]1338;am-shell-started\x07\x1b]1338;am-exit-status;0\x07");
        assert_eq!(
            events,
            vec![SentinelEvent::ShellStarted, SentinelEvent::ExitStatus(0)]
        );
        assert!(output.is_empty());
    }

    #[test]
    fn parse_osc_1338_passes_through_unrelated_bytes() {
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"hello\nworld\n");
        assert!(events.is_empty());
        assert_eq!(output, b"hello\nworld\n");
    }

    #[test]
    fn parse_osc_1338_passes_through_osc_1337() {
        // OSC 1337 frames belong to the TaskComplete parser; this
        // parser must not consume them. They surface as raw bytes.
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"\x1b]1337;am-task-complete;info;c2hpcHBlZA==\x07");
        assert!(events.is_empty());
        assert_eq!(output, b"\x1b]1337;am-task-complete;info;c2hpcHBlZA==\x07");
    }

    #[test]
    fn parse_osc_1338_malformed_payload_ignored() {
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"\x1b]1338;bogus-payload\x07");
        assert!(events.is_empty());
        assert!(output.is_empty());
    }

    #[test]
    fn parse_osc_1338_st_terminator_works() {
        // ESC \ (ST) is the alternate OSC terminator per ECMA-48.
        let mut p = SentinelParser::new();
        let (events, output) = p.feed(b"\x1b]1338;am-shell-started\x1b\\");
        assert_eq!(events, vec![SentinelEvent::ShellStarted]);
        assert!(output.is_empty());
    }

    #[test]
    fn init_control_master_dir_creates_with_0700() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = init_control_master_dir(tmp.path()).expect("init");
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn cleanup_stale_master_sockets_removes_old_sock_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = init_control_master_dir(tmp.path()).unwrap();
        let recent = dir.join("recent.sock");
        let stale = dir.join("stale.sock");
        std::fs::write(&recent, b"not really a socket").unwrap();
        std::fs::write(&stale, b"not really a socket").unwrap();
        // Back-date the stale socket's mtime to 2 hours ago.
        let old = filetime::FileTime::from_unix_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 7200,
            0,
        );
        filetime::set_file_mtime(&stale, old).unwrap();
        let removed = cleanup_stale_master_sockets(&dir).unwrap();
        assert_eq!(removed, 1, "exactly one stale socket should be removed");
        assert!(recent.exists(), "recent socket must survive");
        assert!(!stale.exists(), "stale socket must be removed");
    }

    #[test]
    fn from_app_data_creates_and_validates_control_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stub_ssh = tmp.path().join("stub-ssh");
        std::fs::write(&stub_ssh, b"#!/bin/sh\nexit 0\n").unwrap();
        let t = SshTransport::from_app_data(stub_ssh, tmp.path())
            .expect("from_app_data must succeed on fresh dir");
        assert!(t.control_dir.is_dir());
        assert!(t.control_dir.ends_with("ssh-cm"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&t.control_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "must enforce 0700");
        }
    }

    #[test]
    fn from_app_data_runs_stale_cleanup_at_construction() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Pre-seed the ssh-cm dir with an old socket.
        let control_dir = tmp.path().join("ssh-cm");
        std::fs::create_dir_all(&control_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&control_dir).unwrap().permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&control_dir, perms).unwrap();
        }
        let stale = control_dir.join("stale.sock");
        std::fs::write(&stale, b"x").unwrap();
        let old = filetime::FileTime::from_unix_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 7200,
            0,
        );
        filetime::set_file_mtime(&stale, old).unwrap();

        let stub_ssh = tmp.path().join("stub-ssh");
        std::fs::write(&stub_ssh, b"#!/bin/sh\nexit 0\n").unwrap();
        let _t = SshTransport::from_app_data(stub_ssh, tmp.path()).expect("from_app_data");
        assert!(
            !stale.exists(),
            "from_app_data must remove stale sockets at construction"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_control_socket_owner_safe_passes_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.sock");
        ensure_control_socket_owner_safe(&missing).expect("missing file is safe");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_control_socket_owner_safe_passes_when_owned_by_us() {
        let tmp = tempfile::TempDir::new().unwrap();
        let owned = tmp.path().join("ours.sock");
        std::fs::write(&owned, b"x").unwrap();
        ensure_control_socket_owner_safe(&owned).expect("our own file is safe");
    }

    #[test]
    fn cleanup_stale_master_sockets_ignores_non_sock_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = init_control_master_dir(tmp.path()).unwrap();
        let other = dir.join("ignored.txt");
        std::fs::write(&other, b"x").unwrap();
        cleanup_stale_master_sockets(&dir).unwrap();
        assert!(other.exists(), "non-sock files must be left alone");
    }

    #[test]
    fn cleanup_stale_master_sockets_returns_zero_for_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let removed = cleanup_stale_master_sockets(&tmp.path().join("does-not-exist")).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn ssh_location_from_workspace_extracts_remote_fields() {
        let ws = WorkspaceLocation::Remote {
            user: Some("alice".into()),
            host: "h".into(),
            port: Some(2222),
            canonical_remote_path: "/srv".into(),
            container: None,
        };
        let loc = SshLocation::from_workspace(&ws).expect("remote");
        assert_eq!(loc.user.as_deref(), Some("alice"));
        assert_eq!(loc.host, "h");
        assert_eq!(loc.port, Some(2222));
    }

    #[test]
    fn ssh_location_from_workspace_returns_none_for_local() {
        let ws = WorkspaceLocation::Local { path: None };
        assert!(SshLocation::from_workspace(&ws).is_none());
    }

    /// Race-window regression: when ssh's child exits before the
    /// reader thread parses `am-shell-started`, the drain helper
    /// MUST observe the sentinel that arrives during the drain
    /// window. Run 20 iterations with the sentinel arriving
    /// ~5-20ms after the drain starts; assert every iteration
    /// returns `true` (pre-shell handshake actually completed).
    #[test]
    fn drain_and_recheck_observes_sentinel_set_during_drain_window() {
        for iter in 0..20 {
            let shared = Arc::new(SharedSshState::new());
            let shared_for_setter = Arc::clone(&shared);
            // Set the sentinel a few ms after the drain starts to
            // exercise the in-loop observation path.
            let delay_ms = 5 + (iter % 16);
            let setter = thread::spawn(move || {
                thread::sleep(Duration::from_millis(delay_ms));
                shared_for_setter.note_shell_started();
            });
            let observed =
                drain_and_recheck_shell_started(&shared, Duration::from_millis(500));
            setter.join().unwrap();
            assert!(
                observed,
                "iter {iter}: sentinel set during drain MUST be observed (delay={delay_ms}ms)"
            );
        }
    }

    /// Late-arrival case: the reader stores the sentinel and
    /// then sets `reader_done` (mirroring the real reader thread
    /// after EOF). The helper's last-chance recheck MUST observe
    /// the sentinel — the previous code returned without
    /// rechecking, which was the bug.
    #[test]
    fn drain_and_recheck_observes_sentinel_then_reader_done() {
        let shared = Arc::new(SharedSshState::new());
        // Pre-set BOTH so the in-loop check at iteration 1
        // exits via shell_started before the loop sees
        // reader_done. This pins the "reader finished and
        // sentinel was set" composite state.
        shared.note_shell_started();
        shared.reader_done.store(true, Ordering::SeqCst);
        let observed =
            drain_and_recheck_shell_started(&shared, Duration::from_millis(500));
        assert!(observed, "reader-done + sentinel-set MUST classify as ShellStarted");
    }

    /// Genuine failure: reader finished without ever seeing the
    /// sentinel. The helper must return `false` so the caller
    /// classifies the exit as `FailedBeforeShell`.
    #[test]
    fn drain_and_recheck_returns_false_when_reader_done_without_sentinel() {
        let shared = Arc::new(SharedSshState::new());
        shared.reader_done.store(true, Ordering::SeqCst);
        let observed =
            drain_and_recheck_shell_started(&shared, Duration::from_millis(100));
        assert!(
            !observed,
            "reader done without sentinel must classify as pre-shell failure"
        );
    }

    /// Deadline timeout: neither sentinel nor reader-done set.
    /// Helper returns `false` after the deadline elapses.
    #[test]
    fn drain_and_recheck_returns_false_after_deadline_with_no_signals() {
        let shared = Arc::new(SharedSshState::new());
        let start = Instant::now();
        let observed =
            drain_and_recheck_shell_started(&shared, Duration::from_millis(40));
        let elapsed = start.elapsed();
        assert!(!observed, "no signals → no sentinel observed");
        assert!(
            elapsed >= Duration::from_millis(40),
            "helper must wait the full deadline; elapsed={elapsed:?}"
        );
        // Upper bound check — the loop polls at 5ms intervals so
        // some slack is fine, but a runaway loop would blow past
        // 500ms.
        assert!(
            elapsed < Duration::from_millis(500),
            "helper must not exceed the deadline by more than poll-jitter; elapsed={elapsed:?}"
        );
    }
}
