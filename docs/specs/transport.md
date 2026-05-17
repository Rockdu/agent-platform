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

When authentication fails under `BatchMode=yes`, classify it as `TransportError::SshAuth` and surface `AttentionKind::Disconnect`.

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
printf '\033]1338;am-shell-started\a'

status=0

if [ -n "$AM_REMOTE_CWD" ]; then
  cd "$AM_REMOTE_CWD" || exit_status=$?
fi

if [ -n "$exit_status" ]; then
  status="$exit_status"
else
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

The actual implementation may inline this script through `sh -lc` or upload it to a temporary remote file, but the emitted OSC protocol is fixed.

Parser behavior:

- Seeing `am-shell-started` sets `shell_started = true`.
- Seeing `am-exit-status;N` sets `remote_exit_status = N`.
- Sentinel sequences are consumed as control markers and should not render visibly in the terminal.
- Malformed sentinel payloads are ignored and may be logged at debug level.

### 4.6 SSH state-machine mapping

| shell_started seen? | exit_status sentinel seen? | ssh exit | Resulting AttentionKind / typed error |
|---|---:|---:|---|
| yes | `0` | `0` | `AttentionKind::Completion` / `TransportExitStatus::CleanCompletion` |
| yes | non-zero `N` | any | `AttentionKind::NonZeroExit` / `TransportExitStatus::NonZeroExit(N)` |
| yes | no | non-zero or signal | `AttentionKind::Disconnect` / `TransportError::RemoteCommandFailed` or `DisconnectReason::Io` |
| no | no | auth failure pattern | `AttentionKind::Disconnect` / `TransportError::SshAuth` |
| no | no | connect timeout, DNS, refused, no route | `AttentionKind::Disconnect` / `TransportError::SshConnect` |
| no | no | host key changed pattern | `AttentionKind::Disconnect` / `TransportError::SshHostKeyChanged` |

Auth, connect, and host-key classification can initially use OpenSSH stderr pattern matching plus process exit status. The classification must be centralized in `crates/terminal-mesh-transport-ssh/src/ssh.rs` so it can be hardened later.

### 4.7 Error taxonomy

`TransportError` should live in `crates/terminal-mesh-core/src/transport.rs` if shared broadly, or in `terminal-mesh-transport-ssh` with conversion into core status if kept transport-specific.

Required variants:

```rust
pub enum TransportError {
    SpawnFailed { program: String, message: String },
    Io { message: String },
    ResizeFailed { message: String },
    ShutdownFailed { message: String },
    WaitFailed { message: String },

    SshBinaryNotFound,
    SshAuth { user: String, host: String, port: u16 },
    SshConnect { host: String, port: u16, message: String },
    SshHostKeyChanged { host: String, port: u16 },
    SshShellDidNotStart { host: String, port: u16, ssh_exit: Option<i32> },
    SshControlPathInvalid { path: PathBuf, message: String },

    DockerContainerMissing { container: String },
    DockerExecFailed { container: String, message: String },
    DockerCleanupFailed { container: String, message: String },

    Protocol { message: String },
}
```

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
