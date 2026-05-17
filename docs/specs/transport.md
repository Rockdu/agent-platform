# Transport — Local, SSH, and SSH-in-Docker PTY transports

## 1. Overview / Goals

The desktop agent platform needs a transport abstraction at the PTY spawn boundary so one terminal tab can be backed by a local PTY, a shell reached through `ssh`, or a shell reached through `ssh` plus `docker exec`. The abstraction must preserve the existing terminal-mesh behavior: local scrollback buffering, tab indexing in `src-tauri/src/terminal_mesh.rs`, host notification dedup in `src-tauri/src/notification.rs`, capability checks in `src-tauri/src/dispatcher.rs`, and existing `terminal-mesh-core` attention parsing.

This spec adds a lifecycle-complete `Transport` / `TransportSession` contract rather than a spawn-only wrapper. Remote transports need explicit stdin/stdout ownership, resize propagation, shutdown, wait, disconnect classification, and cleanup semantics. The first implementation targets these new locations:

- `crates/terminal-mesh-core/src/transport.rs`
- `crates/terminal-mesh-transport-ssh/src/lib.rs`
- `crates/terminal-mesh-transport-ssh/src/ssh.rs`
- `crates/terminal-mesh-transport-ssh/src/docker_over_ssh.rs`
- `crates/terminal-mesh-transport-ssh/src/sentinel.rs`
- `crates/terminal-mesh-transport-ssh/src/control_master.rs`
- `src-tauri/src/workspaces.rs`
- `src-tauri/src/terminal_mesh.rs`

## 2. Transport trait contract

`Transport` is responsible for starting a PTY-like interactive session. `TransportSession` owns the resulting lifecycle.

```rust
pub trait Transport: Send + Sync {
    fn spawn(
        &self,
        request: TransportSpawnRequest,
    ) -> Result<Box<dyn TransportSession>, TransportError>;
}

pub struct TransportSpawnRequest {
    pub workspace: WorkspaceLocation,
    pub command: ShellCommand,
    pub initial_size: PtySize,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBufOrRemote>,
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
```

`output_stream()` returns the byte stream read by terminal-mesh-core. This remains the single source for terminal rendering, local scrollback, OSC marker parsing, completion detection, and attention events.

`stdin_sink()` returns the write side used by keyboard input, paste, and injected commands. It must be independent from the output stream so local and remote sessions can share the same terminal-mesh tab machinery.

`resize()` propagates terminal dimensions. Local transport delegates to the existing PTY resize call. SSH transport must resize the local ssh process PTY; the remote shell receives the resulting window-change signal through OpenSSH.

`shutdown()` requests termination without waiting. `Graceful` should send EOF or a terminal interrupt when supported. `Kill` must terminate the local transport process and perform transport-specific cleanup, such as Docker wrapper PID cleanup.

`wait()` blocks until the underlying session exits and returns a typed status. It must distinguish shell exit, ssh process exit, and transport disconnect where possible.

`disconnect_reason()` returns the best known transport-layer failure after `wait()` or a failed read/write. This supports the new `AttentionKind::Disconnect`.

`cleanup()` is idempotent and must release side resources that are not guaranteed to disappear when the process exits. For SSH this includes stale ControlMaster socket handling. For Docker-over-SSH this includes wrapper PID cleanup.

```rust
pub enum TransportExitStatus {
    CleanCompletion,
    NonZeroExit(i32),
    Signaled(i32),
    Disconnect(DisconnectReason),
}

pub enum ShutdownMode {
    Graceful,
    Kill,
}

pub enum DisconnectReason {
    SshAuth,
    SshConnect,
    SshHostKeyChanged,
    SshProcessExitedBeforeShell,
    RemoteCommandFailed,
    Io(String),
    Unknown(String),
}
```

The terminal attention mapping is:

- `TransportExitStatus::CleanCompletion` -> `AttentionKind::Completion`
- `TransportExitStatus::NonZeroExit(_)` -> `AttentionKind::NonZeroExit`
- `TransportExitStatus::Signaled(_)` -> `AttentionKind::NonZeroExit`
- `TransportExitStatus::Disconnect(_)` -> `AttentionKind::Disconnect`

The done queue triggers only on:

- `AttentionKind::Completion`
- `AttentionKind::NonZeroExit`
- `AttentionKind::Disconnect`
- `AttentionKind::TaskComplete { summary }`

`AttentionKind::AgentMarker` and `AttentionKind::PromptWaiting` must not enqueue done work.

## 3. LocalTransport

`LocalTransport` is a thin adapter over the existing `native_pty_system()` path in `terminal-mesh-core`. It must preserve current behavior exactly for local tabs:

- portable-pty backed spawn
- existing `MasterPty` trait-object ownership
- existing `ChildKiller` trait-object ownership
- existing output buffering and scrollback behavior
- existing resize behavior
- existing completion and non-zero-exit behavior
- existing OSC 1337 `AgentMarker` parsing

Recommended location:

- `crates/terminal-mesh-core/src/transport.rs`
- `crates/terminal-mesh-core/src/local_transport.rs`

The first local implementation should be intentionally boring: adapt current spawn return values into `TransportSession` without changing local process semantics.

## 4. SshTransport

### 4.1 System ssh invocation argv template

Remote shells use the system `ssh` binary. No embedded Rust SSH library is used in v1.

Template:

```text
ssh
  -tt
  -o BatchMode=yes
  -o ControlMaster=auto
  -o ControlPersist=yes
  -o ControlPath=${APP_DATA}/ssh-cm/<hash>.sock
  -o StrictHostKeyChecking=accept-new
  -o ConnectTimeout=10
  -p <port>
  <user>@<host>
  <remote-wrapper-command>
```

Flag rationale:

- `-tt`: forces TTY allocation even when ssh stdin/stdout are pipes from the app. The remote command is an interactive shell session and must behave like a terminal.
- `BatchMode=yes`: disables password and passphrase prompts. v1 is public-key only.
- `ControlMaster=auto`: reuses an existing master connection when available and creates one when needed.
- `ControlPersist=yes`: allows the master connection to remain available briefly after a session exits. The exact persist lifetime can be refined, but the path and cleanup contract are fixed.
- `ControlPath=...`: stores the multiplexing socket under app data using a bounded hashed filename.
- `StrictHostKeyChecking=accept-new`: automatically trusts first-time hosts, while still failing on host key changes.
- `ConnectTimeout=10`: initial reference timeout for connection establishment.
- `-p <port>`: explicit port from `WorkspaceLocation`; default is `22` when omitted.
- `<remote-wrapper-command>`: shell-quoted wrapper script that emits sentinel OSC sequences for shell start and exit status.

### 4.2 ControlMaster and ControlPath layout

Control sockets live under:

```text
${APP_DATA}/ssh-cm/<hash>.sock
```

`<hash>` is computed from the canonical SSH connection identity:

```text
ssh://<user>@<host>:<port>
```

Use a stable cryptographic hash such as SHA-256 encoded as lowercase hex, truncated only if the full filename would be too long. The filename must be bounded to avoid Unix socket path-length failures. The hash input must not include the remote workspace path or container name because ControlMaster reuse is per SSH endpoint, not per workspace.

Directory contract:

- Create `${APP_DATA}/ssh-cm` on bootstrap.
- Directory permissions must be `0700`.
- Refuse to use the directory if it is not owned by the current user.
- Best-effort set socket permissions to `0600` where the platform exposes socket file modes.
- Refuse to use an existing socket path that is not owned by the current user.
- Do not place ControlPath sockets in workspace directories.

Stale-master cleanup contract on bootstrap:

1. Enumerate `${APP_DATA}/ssh-cm/*.sock`.
2. For each socket owned by the current user, attempt an OpenSSH control check when enough metadata exists, or attempt a conservative socket liveness check.
3. Remove sockets that are definitely stale.
4. Leave ambiguous live sockets in place.
5. Never remove files outside `${APP_DATA}/ssh-cm`.

The cleanup helper should live in:

- `crates/terminal-mesh-transport-ssh/src/control_master.rs`

### 4.3 BatchMode key-only auth contract

v1 supports public-key authentication only. The app must not show password or passphrase prompts and must not attempt keyboard-interactive auth.

Users must pre-arrange one of:

- a running `ssh-agent` with the needed key loaded
- an unencrypted key accepted by their OpenSSH config
- host-specific OpenSSH config in `~/.ssh/config`

When authentication fails under `BatchMode=yes`, `Transport::spawn` MUST return `Err(TransportError::SshAuth { user, host, port })` synchronously. No `TransportSession` handle is produced, no `AttentionKind` event is emitted, no `WorkspaceLifecycleSnapshot` Done entry is enqueued, and no `WorkspaceRecord` is persisted. The caller (WorkspaceLaunchScheduler / workspace-create path) surfaces the typed error to the UI as a create-time failure, not as a lifecycle disconnect.

Rationale: `AttentionKind::Disconnect` is reserved for sessions that successfully started a remote shell and later lost their transport. A workspace that never reached `am-shell-started` has no session to "disconnect" and must not pollute the Done queue with phantom entries the user did not initiate.

### 4.4 `StrictHostKeyChecking=accept-new`

The v1 behavior is intentionally named as `accept-new`, not “strict”.

Semantics:

- First connection to an unknown host auto-trusts and records the host key through OpenSSH.
- A changed host key still fails.
- A host key change maps to `TransportError::SshHostKeyChanged`.
- v2 may add an in-app fingerprint confirmation UI before first trust.

The UI and logs must avoid saying that this is strict host checking. It is first-connect auto-trust with change detection.

### 4.5 Sentinel protocol

SSH exit status alone is not enough to distinguish “remote shell exited” from “ssh failed before the shell started.” v1 uses a wrapper-shell sentinel protocol, not banner-line heuristics.

Sentinel OSC sequences:

```text
ESC ] 1338 ; am-shell-started BEL
ESC ] 1338 ; am-exit-status ; <decimal-status> BEL
```

In escaped shell form:

```text
\033]1338;am-shell-started\a
\033]1338;am-exit-status;<decimal-status>\a
```

OSC code `1338` is reserved for transport shell lifecycle markers and must not collide with the existing OSC `1337` `AgentMarker` convention.

Remote wrapper snippet:

```sh
#!/bin/sh
# Reserved exit codes (must NOT collide with shell-relayed exit codes):
#   77 = RemotePathInvalid (cd to $AM_REMOTE_CWD failed BEFORE shell start)
# All other non-zero codes are interpreted as the user shell's own exit status.

if [ -n "$AM_REMOTE_CWD" ]; then
  if ! cd "$AM_REMOTE_CWD" 2>/dev/null; then
    # Do NOT emit am-shell-started: we never reached the interactive shell.
    # Host maps exit code 77 -> TransportError::RemotePathInvalid.
    exit 77
  fi
fi

printf '\033]1338;am-shell-started\a'

if [ -n "$SHELL" ] && [ -x "$SHELL" ]; then
  "$SHELL" -l
  status=$?
else
  /bin/sh -l
  status=$?
fi

printf '\033]1338;am-exit-status;%s\a' "$status"
exit "$status"
```

The actual implementation may inline this script through `sh -lc` or upload it to a temporary remote file, but the emitted OSC protocol AND the reserved exit-code semantics (77 = RemotePathInvalid) are fixed. Exit code 77 is chosen because it is outside the conventional shell-builtin exit range (0-2) and the signal-derived range (128+N) and is not assigned by POSIX or common shells, minimizing the risk that a user shell coincidentally returns it. If a host observes ssh exit code 77 AND no `am-shell-started` sentinel was seen, it MUST classify the failure as `TransportError::RemotePathInvalid`.

Parser behavior:

- Seeing `am-shell-started` sets `shell_started = true`.
- Seeing `am-exit-status;N` sets `remote_exit_status = N`.
- Sentinel sequences are consumed as control markers and should not render visibly in the terminal.
- Malformed sentinel payloads are ignored and may be logged at debug level.

### 4.6 SSH state-machine mapping

Two distinct phases govern classification:

**Phase A — Pre-shell (`Transport::spawn` time).** Failures that occur before the wrapper emits `am-shell-started` MUST surface as typed `TransportError` returned synchronously from `Transport::spawn` (`Err(...)`). No `TransportSession` is constructed, no `AttentionKind` event is emitted, no Done entry is enqueued, and no `WorkspaceRecord` is persisted. The caller (workspace-create or WorkspaceLaunchScheduler) presents these as create-time errors.

**Phase B — Post-shell (during an established session).** Failures or completions that occur after `am-shell-started` was seen MUST surface through the normal `TransportSession::wait` / `disconnect_reason` path and may emit `AttentionKind` events that DO produce Done-queue entries.

| Row | shell_started seen? | exit_status sentinel seen? | ssh exit / failure | Phase | Surface | Resulting classification | AttentionKind | Done entry? |
|---|---|---:|---|---|---|---|---|---|
| 1 | yes | `0` | `0` | B | `wait` returns Ok | `TransportExitStatus::CleanCompletion` | `Completion` | yes |
| 2 | yes | non-zero `N` | any | B | `wait` returns Ok | `TransportExitStatus::NonZeroExit(N)` | `NonZeroExit` | yes |
| 3 | yes | no | non-zero or signal | B | `wait` returns Err / `disconnect_reason = Io` | session-side disconnect | `Disconnect` | yes |
| 4 | no | no | OpenSSH stderr matches auth failure pattern (`Permission denied`, `publickey`) | A | `spawn` returns `Err` | `TransportError::SshAuth { user, host, port }` | (none — no session created) | **no** |
| 5 | no | no | exit before any TCP handshake (refused, DNS, no route, timeout) | A | `spawn` returns `Err` | `TransportError::SshConnect { host, port, message }` | (none) | **no** |
| 6 | no | no | OpenSSH stderr matches host-key-changed pattern (`REMOTE HOST IDENTIFICATION HAS CHANGED`) | A | `spawn` returns `Err` | `TransportError::SshHostKeyChanged { host, port, message }` | (none) | **no** |
| 7 | no | no | ssh exit code `77` | A | `spawn` returns `Err` | `TransportError::RemotePathInvalid { path, host, port, message }` | (none) | **no** |
| 8 | no | no | ssh exits cleanly OR with another non-zero code without matching any pattern above | A | `spawn` returns `Err` | `TransportError::SshShellDidNotStart { host, port, ssh_exit }` | (none) | **no** |

Notes:

- Rows 4-8 are all Phase A: pre-shell, typed errors only, NO Done entry, NO `AttentionKind` event. A failed-to-start SSH workspace must never appear in the user's Running or Done sidebar — only as a create-time error toast / dialog.
- Row 3 is the only Phase B "disconnect" — the user already had a live remote shell that the network lost.
- The Phase A classifier MUST inspect stderr patterns BEFORE falling through to the catch-all `SshShellDidNotStart` so that host-key changes and auth failures are reported precisely.
- The exit-code-77 → `RemotePathInvalid` mapping (row 7) is contingent on the wrapper snippet in §4.5 being used. If a custom wrapper is substituted, the classifier MUST still fall through to `SshShellDidNotStart` when shell_started was not seen.
- The classification logic must be centralized in `crates/terminal-mesh-transport-ssh/src/ssh.rs` so it can be hardened later (regex patterns, OpenSSH version differences).

### 4.7 Error taxonomy

`TransportError` lives in `crates/terminal-mesh-core/src/transport.rs` (shared across LocalTransport, SshTransport, and DockerOverSshTransport). Variants prefixed `Ssh*` and `Docker*` are scaffolded in the core crate but only produced by the SSH/Docker transport crates.

All variants in the SSH/Docker pre-shell group (rows 4-8 of §4.6) are Phase A: they are returned synchronously from `Transport::spawn` as `Err(...)`. They MUST NOT be wrapped in a `TransportSession` and MUST NOT trigger any `AttentionKind` event. Workspaces that fail with these errors never enter the Running or Done sidebars.

Required variants:

```rust
pub enum TransportError {
    // ---- LocalTransport / generic transport errors ----
    SpawnFailed { program: String, message: String },
    Io { message: String },
    ResizeFailed { message: String },
    ShutdownFailed { message: String },
    WaitFailed { message: String },

    // ---- SshTransport pre-shell errors (Phase A: returned from spawn, NO AttentionKind, NO Done entry) ----
    SshBinaryNotFound,
    SshAuth { user: String, host: String, port: u16 },
    SshConnect { host: String, port: u16, message: String },
    SshHostKeyChanged { host: String, port: u16, message: String },
    SshShellDidNotStart { host: String, port: u16, ssh_exit: Option<i32>, stderr_tail: String },
    SshControlPathInvalid { path: PathBuf, message: String },
    RemotePathInvalid { path: String, host: String, port: u16, message: String },

    // ---- DockerOverSshTransport errors ----
    DockerContainerMissing { container: String },
    DockerExecFailed { container: String, message: String },
    DockerCleanupFailed { container: String, message: String },

    // ---- Catch-all ----
    Protocol { message: String },
}
```

`RemotePathInvalid` is produced when the wrapper of §4.5 exits with code `77` (cd to `$AM_REMOTE_CWD` failed before reaching the interactive shell). The `path` field carries the offending `$AM_REMOTE_CWD` value; `message` carries a brief human-readable description (typically derived from `cd`'s stderr, e.g. `"No such file or directory"` or `"Permission denied"`).

`SshHostKeyChanged` includes `message` so the UI can surface the OpenSSH-emitted hint (`"REMOTE HOST IDENTIFICATION HAS CHANGED!"` plus the offending known_hosts line number) to help the user diagnose the change.

`SshShellDidNotStart` is the catch-all for "ssh exited with no shell-started sentinel and stderr matched no known failure pattern"; `stderr_tail` carries up to the last 2 KiB of stderr to aid post-mortem.

## 5. DockerOverSshTransport

### 5.1 Composition with SshTransport

`DockerOverSshTransport` composes `SshTransport`. It does not implement a separate network stack.

Remote command shape:

```text
docker exec -it <container> /bin/sh -lc '<wrapper-script>'
```

The SSH layer remains responsible for:

- ControlMaster reuse
- host key behavior
- key-only auth
- local PTY management
- shell-started and exit-status sentinel parsing

Docker-over-SSH adds the container target and cleanup contract.

### 5.2 Wrapper-PID cleanup

The wrapper script must record the PID of the process that owns the inner shell lifecycle. A deterministic PID file path is created inside the existing container:

```text
/tmp/agentmesh-wrapper-<session-id>.pid
```

Container wrapper snippet:

```sh
#!/bin/sh
pid_file="/tmp/agentmesh-wrapper-${AM_SESSION_ID}.pid"
printf '%s\n' "$$" > "$pid_file"

cleanup_pid_file() {
  rm -f "$pid_file"
}

trap cleanup_pid_file EXIT HUP INT TERM

printf '\033]1338;am-shell-started\a'

status=0
if [ -n "$AM_REMOTE_CWD" ]; then
  cd "$AM_REMOTE_CWD" || status=$?
fi

if [ "$status" -eq 0 ]; then
  if [ -n "$SHELL" ] && [ -x "$SHELL" ]; then
    "$SHELL" -l
  else
    /bin/sh -l
  fi
  status=$?
fi

printf '\033]1338;am-exit-status;%s\a' "$status"
exit "$status"
```

On `shutdown(Kill)`, the app sends a cleanup command over the same SSH endpoint:

```text
ssh <same-controlmaster-options> <user>@<host> \
  docker exec <container> sh -lc 'kill "$(cat /tmp/agentmesh-wrapper-<session-id>.pid)" 2>/dev/null || true; rm -f /tmp/agentmesh-wrapper-<session-id>.pid'
```

Do not use `docker stop`. The app attaches to an existing user-managed container and must not stop or destroy it.

### 5.3 Fd-leak regression test contract

Docker-over-SSH must have a regression test contract proving cleanup of the wrapper process and `docker exec` process tree.

Test requirement:

- Start a Docker-over-SSH session against a fixture or mocked command runner.
- Call `shutdown(Kill)`.
- Wait up to 5 seconds.
- Assert no orphan `docker exec` process for the session remains.
- Assert the wrapper PID file is removed or cleanup was attempted and failure was reported as `DockerCleanupFailed`.

If full Docker and SSH integration is not available in unit tests, use a fake command runner that records process lifetimes and validates the cleanup command exactly.

### 5.4 Existing-container-only semantics

v1 never creates, starts, stops, removes, or mutates containers.

Allowed:

- `docker exec -it <container> ...`
- `docker exec <container> kill <pid>`
- `docker exec <container> rm -f <pid-file>`

Not allowed:

- `docker run`
- `docker create`
- `docker start`
- `docker stop`
- `docker rm`
- image pulls or builds

A missing container maps to `TransportError::DockerContainerMissing`.

## 6. WorkspaceLocation

### 6.1 Identity tuple

`WorkspaceLocation` replaces local-only identity with explicit transport identity.

```rust
pub enum WorkspaceLocation {
    Local {
        path: PathBuf,
    },
    Remote {
        scheme: RemoteScheme,
        user: Option<String>,
        host: String,
        port: Option<u16>,
        canonical_remote_path: String,
        container: Option<String>,
    },
}

pub enum RemoteScheme {
    Ssh,
    SshDocker,
}
```

Identity tuple:

```text
{
  scheme: "local" | "ssh" | "ssh+docker",
  user?,
  host?,
  port?,
  canonical_remote_path?,
  container?
}
```

For local records, identity remains path plus inode duplicate detection.

For remote records:

- `scheme` is required.
- `host` is required.
- `port` defaults to `22` for identity if omitted.
- `canonical_remote_path` is required.
- `container` is required for `ssh+docker` and absent for plain `ssh`.
- `user` is part of identity when present.

### 6.2 Duplicate-detection rules

Local duplicate detection remains inode-based in `src-tauri/src/workspaces.rs` using the existing `WorkspaceRecord { path: PathBuf, ... }` behavior.

Remote duplicate detection uses tuple equality only:

```text
scheme + user + host + normalized_port + canonical_remote_path + container
```

Do not attempt inode checks for remote paths. Do not treat two different host aliases as duplicates unless their identity tuple is equal after canonicalization performed by the app.

### 6.3 Migration

Existing workspace records deserialize as local records:

```rust
WorkspaceLocation::Local { path: record.path }
```

Backward compatibility requirements:

- Existing serialized records with `path: PathBuf` remain valid.
- New records include `location`.
- During deserialization, missing `location` plus present `path` means local.
- Existing UI and commands that only understand local paths must either receive a local path or return a typed unsupported-remote error.

## 7. DEC-9 scope clarification

### 7.1 Remote workspaces are shell-only in v1

Remote workspaces provide interactive shell transport only. They do not deploy the app sidecar, MCP servers, generated MCP configs, or remote capability services.

### 7.2 Local scrollback still works

`terminal_mesh.read_scrollback` continues working against remote tabs because scrollback is buffered locally by terminal-mesh-core after bytes arrive over the transport stream.

No remote-side scrollback service is required for v1.

### 7.3 Remote claude MCP behavior

If the user runs `claude` inside a remote SSH or SSH-in-Docker tab, that process uses its own remote environment, including its own `~/.claude/`.

The app does not generate or inject per-workspace MCP config for remote claude in v1.

### 7.4 Why含义 2 remote sidecar deployment is v2

Remote sidecar deployment is queued for v2 because it is roughly 3-5x the implementation cost of shell-only transport. It requires at least:

- binary distribution for remote OS and architecture combinations
- remote install, upgrade, and cleanup policy
- socket forwarding or equivalent RPC transport
- MCP config path rewriting for remote filesystems
- remote capability and auth boundary review
- debugging and support tooling for partial remote installs

v1 intentionally avoids that surface.

## 8. TaskComplete OSC marker convention

### 8.1 OSC format

`TaskComplete` uses OSC `1339`, distinct from existing OSC `1337` `AgentMarker` and OSC `1338` transport lifecycle sentinels.

Format:

```text
ESC ] 1339 ; am-task-complete ; <severity> ; <base64-summary> BEL
```

Escaped form:

```text
\033]1339;am-task-complete;<severity>;<base64-summary>\a
```

`severity` values:

- `info`
- `success`
- `warning`
- `error`

`summary` is UTF-8 text encoded with standard base64. Empty summaries are allowed but should be ignored by the done queue unless the caller explicitly needs an empty completion marker.

Example:

```text
\033]1339;am-task-complete;success;QnVpbGQgZmluaXNoZWQu\a
```

### 8.2 Parser behavior

`terminal-mesh-core` currently has `OscAgentMarkerParser` for OSC `1337` agent markers. Extend this parser or split it into a more general OSC attention parser in:

- `crates/terminal-mesh-core/src/osc_agent_marker.rs`
- or `crates/terminal-mesh-core/src/attention_parser.rs`

Required behavior:

- OSC `1337` continues emitting `AttentionKind::AgentMarker`.
- OSC `1338` shell lifecycle markers update transport sentinel state and are not exposed as agent markers.
- OSC `1339;am-task-complete;...` emits `AttentionKind::TaskComplete { summary }`.
- Invalid base64, invalid UTF-8, unknown severity, or malformed field counts are ignored and logged at debug level.
- Parsed marker bytes should not render visibly in the terminal.

`AttentionKind` must add:

```rust
pub enum AttentionKind {
    Completion,
    NonZeroExit,
    PromptWaiting,
    AgentMarker,
    Disconnect,
    TaskComplete { summary: String },
}
```

`AgentMarker` remains for in-progress markers. It must not be repurposed as task completion.

### 8.3 Trust boundary

Any process that can write to a workspace PTY can emit OSC markers. This is the same low-trust boundary as the existing `AgentMarker` mechanism.

v1 does not sign, authenticate, or cryptographically verify OSC markers. Consumers must treat `TaskComplete` as a UI and workflow hint, not as proof of correctness.

## 9. Non-goals for v1

The following are explicitly queued for v2 or later:

- tmux-backed remote durability
- in-app host fingerprint confirmation UI
- container creation, start, stop, removal, image pull, or image build
- password authentication
- passphrase prompts
- keyboard-interactive authentication
- remote sidecar deployment
- remote MCP config generation
- remote socket forwarding for host RPC
- replacing system `ssh` with an embedded Rust SSH library

## 10. Test contracts

### LocalTransport parity

Unit and integration tests must verify:

- local spawn uses the existing native PTY path
- stdin writes reach the child process
- output bytes are buffered exactly as before
- resize delegates to the local PTY
- clean exit maps to `Completion`
- non-zero exit maps to `NonZeroExit`
- existing OSC `1337` `AgentMarker` tests still pass

Recommended test location:

- `crates/terminal-mesh-core/tests/local_transport.rs`

### SshTransport sentinel parsing

Tests must cover:

- `am-shell-started` followed by `am-exit-status;0`
- `am-shell-started` followed by `am-exit-status;42`
- ssh exits before `am-shell-started`
- malformed sentinel payloads
- split OSC sequences across multiple read chunks
- multiple OSC sequences in one chunk
- OSC `1338` does not collide with OSC `1337`
- ControlPath hash is stable and bounded
- ControlPath directory permission checks reject unsafe ownership or mode

Recommended test locations:

- `crates/terminal-mesh-transport-ssh/tests/sentinel.rs`
- `crates/terminal-mesh-transport-ssh/tests/control_master.rs`
- `crates/terminal-mesh-transport-ssh/tests/ssh_state_machine.rs`

### SshTransport Phase A negative paths (pre-shell failures)

These tests assert the §4.6 Phase A invariant: failures before `am-shell-started` produce a typed `TransportError` from `Transport::spawn` AND do NOT enqueue a Done entry AND do NOT persist a `WorkspaceRecord`.

Each test below MUST assert all THREE conditions:

1. `Transport::spawn(...).await` returns `Err(TransportError::<expected variant with expected fields>)`.
2. After the spawn error, `WorkspaceLifecycleSnapshot::done` is empty (no phantom Done entry was enqueued).
3. After the spawn error, `WorkspaceStore::list()` does not contain a record for the failed workspace (no half-broken record was persisted).

Required negative-path coverage:

- **Auth failure**: simulate ssh exit with stderr containing `Permission denied (publickey)` → `TransportError::SshAuth { user, host, port }` with all three fields populated from the spawn request.
- **Host-key changed**: simulate ssh exit with stderr containing `REMOTE HOST IDENTIFICATION HAS CHANGED!` → `TransportError::SshHostKeyChanged { host, port, message }` with the stderr line preserved in `message`.
- **Connect failure (no TCP handshake)**: simulate ssh exit with stderr matching connection-refused / DNS / no-route / timeout patterns → `TransportError::SshConnect { host, port, message }`.
- **Remote path invalid**: simulate ssh exit code `77` with NO `am-shell-started` sentinel observed → `TransportError::RemotePathInvalid { path, host, port, message }` with `path` equal to the requested `AM_REMOTE_CWD`.
- **Catch-all (`SshShellDidNotStart`)**: simulate ssh exit with an unknown non-zero code and no recognized stderr pattern → `TransportError::SshShellDidNotStart { host, port, ssh_exit, stderr_tail }`.
- **Phase A vs Phase B disambiguation**: simulate `am-shell-started` followed by a network drop (no `am-exit-status` sentinel, ssh exits non-zero) → the test MUST observe `Transport::spawn` return `Ok(session)` (Phase A succeeded), then on `session.wait()` see a session-level disconnect, and exactly ONE Done entry tagged with `AttentionKind::Disconnect`. This anchors the boundary between "no Done entry" (Phase A) and "Done entry permitted" (Phase B).

Recommended test location:

- `crates/terminal-mesh-transport-ssh/tests/phase_a_negative.rs`
- `crates/terminal-mesh-transport-ssh/tests/phase_boundary.rs`

### DockerOverSsh wrapper cleanup

Tests must cover:

- remote command is composed as `docker exec -it <container> <wrapper>`
- missing container maps to `DockerContainerMissing`
- `shutdown(Kill)` sends `docker exec <container> kill <pid>` over the same SSH endpoint
- cleanup does not call `docker stop`
- no orphan `docker exec` process remains 5 seconds after shutdown in the regression fixture
- PID file cleanup is attempted even when `kill` fails

Recommended test location:

- `crates/terminal-mesh-transport-ssh/tests/docker_over_ssh.rs`

### WorkspaceLocation

Tests must cover:

- old local records deserialize into `WorkspaceLocation::Local`
- local duplicate detection remains inode-based
- remote duplicate detection uses tuple equality
- `ssh` and `ssh+docker` records with the same host and path are not duplicates
- different container names are different identities
- omitted remote port canonicalizes to `22`

Recommended test location:

- `src-tauri/tests/workspace_location.rs`

### TaskComplete OSC

Tests must cover:

- valid OSC `1339` emits `AttentionKind::TaskComplete { summary }`
- base64 summary decodes as UTF-8
- malformed base64 is ignored
- OSC split across chunks still parses
- TaskComplete triggers done queue enqueue
- AgentMarker and PromptWaiting do not trigger done queue enqueue
- TaskComplete does not render visible marker bytes in terminal output

Recommended test locations:

- `crates/terminal-mesh-core/tests/task_complete_osc.rs`
- `src-tauri/tests/done_queue_attention.rs`
