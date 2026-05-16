# MCP Sidecar Discipline Specification

## Overview

This specification defines the canonical MCP stdio sidecar discipline and lifecycle contract for this project.

It is binding for:

- task10: MCP stdio framing helper crate.
- task11: sidecar lifecycle manager.
- task28/task34: Gmail/Papers sidecar scaffolds.
- Any future plugin sidecar binary that exposes an MCP server.

The project model is OS-process-isolated plugin sidecars. Each plugin sidecar is a native process that exposes an MCP server over stdio transport. The Tauri host owns its host-UI sidecar set. Each `claude` tab fork-execs its own per-claude sidecar set, producing an N x M model: N tabs times M enabled per-claude plugins.

This spec constrains:

- stdio transport framing.
- stdout/stderr/log discipline.
- `client_id` identity and scoping.
- process ownership and lifecycle.
- graceful shutdown and crash recovery.
- restart policy and UI behavior.
- environment variables passed to sidecars.
- lint and integration-test enforcement.

## Transport: MCP Stdio

Sidecars MUST expose MCP over stdio transport. They MUST NOT expose plugin MCP servers over SSE or HTTP for this project lifecycle.

Stdio is required because it matches the `claude` CLI default, requires no port allocation, avoids authentication overhead between local host and child process, and fits the fork-exec lifecycle where the parent process owns stdin, stdout, stderr, PID tracking, and termination.

Per the MCP 2025-11-25 transport specification, MCP stdio uses UTF-8 JSON-RPC messages over standard input and standard output. Messages are newline-delimited JSON-RPC requests, notifications, or responses, and MUST NOT contain embedded newlines. This project therefore uses newline-delimited JSON-RPC framing, not length-prefixed framing. Reference: [MCP 2025-11-25 Transports: stdio](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports).

Channel ownership is absolute:

- `stdin`: host-to-sidecar MCP protocol bytes only.
- `stdout`: sidecar-to-host MCP protocol bytes only.
- `stderr`: log/debug stream.
- Per-process log file: `${APP_DATA}/logs/<plugin_id>/<client_id>.log`, with `client_id` URL-encoded for filename safety.

The host MCP client MUST parse stdout as a strict stream of newline-delimited JSON-RPC messages. The sidecar MUST read stdin as the same framing format.

## Stdout Discipline

Sidecar code may NEVER write to stdout outside the approved MCP framing layer.

This is an absolute rule. No banners, debug lines, progress output, panic summaries, `println!`, child-process inherited stdout, test diagnostics, tracing subscriber output, or dependency logs may reach sidecar stdout.

Enforcement mechanisms:

- Rust sidecar crates MUST deny direct print/debug macros in production code:
  - `println!`
  - `print!`
  - `dbg!`
  - `eprintln!`, except inside approved logging wrappers.
- Cargo workspace lints MUST enable Clippy print macro checks, including `clippy::print_stdout` and `clippy::print_stderr`, with project-specific allowance only for the approved logging wrapper implementation.
- Sidecar logging MUST use approved wrappers such as `agent_log!(level, "...")`, which route only to stderr or the configured per-process log file.
- Integration tests MUST spawn each MVP sidecar and pipe stdout through the strict MCP parser during a 60-second log-heavy test run. The run MUST fail on any framing error, invalid JSON-RPC object, embedded newline, non-protocol line, or unexpected stdout byte sequence.

Failure mode:

If a sidecar writes non-MCP bytes to stdout, the host MCP client sees protocol desynchronization. Because MCP stdio is a single byte stream, even one stray line can be parsed as invalid JSON-RPC or can shift request/response handling into an unrecoverable state.

Recovery behavior:

- Host marks the sidecar transport as corrupt.
- Host records a structured incident log with `plugin_id`, `client_id`, PID, parser error, and the offending byte prefix when safe to record.
- Host terminates the sidecar process.
- Host applies normal crash-restart policy.
- Affected UI surfaces the plugin restart/error state.
- Other sidecars and plugins remain unaffected.

## Log Channel

The default log sink is the per-process file:

```text
${APP_DATA}/logs/<plugin_id>/<urlencoded_client_id>.log
```

`client_id` MUST be URL-encoded for filename safety. For example:

```text
host_ui:notes -> host_ui%3Anotes.log
claude:550e8400-e29b-41d4-a716-446655440000:gmail -> claude%3A550e8400-e29b-41d4-a716-446655440000%3Agmail.log
```

`stderr` is also acceptable for development and debugging. The host MAY capture, forward, or ignore stderr. Sidecars MUST NOT treat stderr as protocol output.

Logs SHOULD be structured JSON lines. Each log event SHOULD include:

- `timestamp`
- `level`
- `message`
- `tab_id`, when applicable.
- `plugin_id`
- `client_id`
- `request_id`, when associated with an MCP request.
- `pid`
- `target` or module name.

Sensitive data MUST be redacted before emission. At minimum, sidecars MUST redact:

- OAuth refresh tokens.
- OAuth access tokens.
- Authorization headers.
- Email bodies.
- OAuth payloads.
- Secret manager payloads.
- Stronghold responses.
- Provider API credentials.
- Cookies and session tokens.

Log rotation is mandatory. Implementations MUST provide at least one of:

- Truncate the active log file when it reaches 10 MB.
- Rotate the active log file to `.log.1` when it reaches 10 MB.

External logrotate integration MAY be used, but the sidecar or host MUST still guarantee that unbounded log growth cannot occur if external logrotate is absent.

## Sidecar Identity Scheme

Each sidecar process has exactly one `client_id`.

Host-owned UI sidecars use:

```text
host_ui:<plugin_id>
```

Per-claude tab sidecars use:

```text
claude:<tab_id>:<plugin_id>
```

`tab_id` values are UUIDv4 strings.

Examples:

```text
host_ui:gmail
host_ui:papers
claude:550e8400-e29b-41d4-a716-446655440000:gmail
claude:550e8400-e29b-41d4-a716-446655440000:papers
```

There is one sidecar PROCESS per `client_id`.

The host `SidecarManager` MUST key sidecar state by `client_id`.

Different `client_id`s mean different:

- OS processes.
- PIDs.
- stdio pipes.
- log files.
- lifecycle FSMs.
- MountRegistry entries.
- capability scopes.

## Lifecycle Manager Contract

`SidecarManager::spawn(plugin_id, client_id, env_vars)` fork-execs the manifest-declared `command_bin`.

Spawn behavior:

- Resolve `command_bin` from the plugin manifest.
- Construct the sidecar environment.
- Open piped stdin/stdout for MCP transport.
- Open stderr according to the configured log mode.
- Register the process PID.
- Register the sidecar under `client_id`.
- Initialize MCP over stdio.
- Attach process monitoring through a Tokio task or equivalent async process watcher.

The host MUST NOT share a sidecar process across different `client_id`s.

Graceful shutdown protocol:

- Host sends a project-defined JSON-RPC request named `mcp/shutdown` over stdio.
- The request means: stop accepting new work, flush in-flight state, close the MCP transport, and exit cleanly.
- The sidecar SHOULD respond successfully before exiting when possible.
- If the sidecar transport is already corrupt or closed, the host MAY skip directly to process termination.

Grace period:

- The sidecar has 5 seconds from `mcp/shutdown` send time to exit cleanly.
- If it does not exit within 5 seconds, the host sends SIGTERM.
- If it remains alive 2 seconds after SIGTERM, the host sends SIGKILL.

Auto-restart:

- On unexpected exit, including non-zero exit code, signal termination, or transport corruption, the manager restarts the sidecar with exponential backoff.
- Backoff sequence is 1s, 2s, 4s, 8s, 16s, 32s, then 60s maximum.
- Backoff resets after 5 minutes of stable uptime.

Restart budget:

- The manager MUST cap restarts at approximately 20 restarts in any 1-hour rolling window per `client_id`.
- Once the budget is exceeded, the sidecar enters an unrecoverable error state.
- Retries stop until the user manually retries or the app restarts.

Crash UI:

- The affected plugin tab or panel shows an error/restarting state.
- The UI includes a manual retry button.
- Other plugins and other sidecar processes remain unaffected.

## Shutdown Sequence (per process)

1. Host sends `mcp/shutdown` request via stdio.
2. Sidecar flushes in-flight state, including pending writes and log buffers.
3. Sidecar closes its MCP transport.
4. Sidecar process exits cleanly with exit code 0.
5. If sidecar does NOT exit within 5 seconds, host sends SIGTERM.
6. If still alive after another 2 seconds, host sends SIGKILL.
7. Host marks the sidecar as terminated in `SidecarManager` and removes its MountRegistry entries.

## Host App Quit Sequence (vs single-sidecar Shutdown)

`AppQuitCoordinator` orchestrates termination of all sidecars, including host-UI sidecars and every active per-claude sidecar.

Quit order:

1. Stop accepting new IPC writes.
2. Resolve or cancel pending confirm-on-write modals.
3. Terminate PTYs according to the Terminal Mesh spec.
4. Send graceful `mcp/shutdown` to all sidecars.
5. Wait 5 seconds for clean exits.
6. Send SIGTERM to remaining sidecars.
7. Wait 2 additional seconds.
8. Send SIGKILL to remaining stragglers.
9. Clear live sidecar registry state.
10. Persist final shutdown diagnostics.

Normal app quit MUST leave no orphan sidecar processes.

On startup, the host MUST scan `${APP_DATA}/sidecar-state/` for stale PID records from crashed or force-quit prior runs. When safe, the host SHOULD reap stale sidecar processes that match this app’s sidecar identity markers. If a PID has been reused by an unrelated process, the host MUST NOT kill it.

## Sidecar Crash Recovery

`SidecarManager` monitors each process through a Tokio task or equivalent watcher. On process exit, the manager classifies the exit as expected or unexpected.

Unexpected exits include:

- Non-zero exit code.
- Signal termination not initiated by the manager.
- stdout framing corruption.
- MCP initialization failure.
- Broken pipe during active use.
- Process death before graceful shutdown completes.

Backoff sequence:

```text
1s, 2s, 4s, 8s, 16s, 32s, 60s, 60s, 60s, ...
```

The cap is 60 seconds.

The backoff counter resets after 5 minutes of stable uptime without restart.

During restart, the plugin UI renders an error state similar to:

```text
Plugin <name> restarting in <next_backoff>s, attempt N. [Retry now]
```

The manual retry button immediately attempts spawn and resets the backoff timer for that `client_id`.

The orchestrator’s `claude` sees a disconnected MCP server during restart. The next MCP request fails with a typed error. `claude` may surface the error to the user, but it MUST NOT crash because a sidecar restarted or disappeared.

## Distinct Identities: host-UI vs per-claude

Host-UI sidecars and per-claude sidecars MUST use distinct identities because they have different ownership, lifetime, authority, and visibility.

Host UI sidecars may be long-lived and authoritative for UI-backed views. For example, a Gmail inbox view may need a host-owned `host_ui:gmail` sidecar that lives independently of any `claude` tab.

Per-claude sidecars are tab-scoped and ephemeral. They are created for a specific `claude` tab and terminated when that tab closes or the app quits.

The identities also differ in capability scope:

- `host_ui:<plugin_id>` sidecars are host-owned and MUST NOT receive `cross_tab_read=true`.
- `claude:<tab_id>:<plugin_id>` sidecars are scoped to one tab.
- A per-claude sidecar such as `claude:<tab_id>:terminal_mesh` MAY receive tab-specific capabilities that are not available to host UI sidecars.
- MountRegistry entries MUST be scoped by exact `client_id`.

`SidecarManager` maintains:

```rust
HashMap<ClientId, ProcessHandle>
```

Each `client_id` has an independent lifecycle FSM.

## Env Vars Passed to Sidecar

The host passes the following environment variables to sidecar processes.

`PLUGIN_ID`:

The plugin identifier from the manifest.

`CLIENT_ID`:

The full sidecar client identifier, such as `host_ui:gmail` or `claude:<tab_id>:gmail`.

`TAB_ID`:

Present only for per-claude sidecars using the `claude:<tab_id>:<plugin_id>` identity form.

`WORKSPACE_PATH`:

Present only for per-claude sidecars. This is the bound workspace directory for the owning tab.

`APP_DATA_DIR`:

Writable per-plugin data directory. Sidecars MUST keep persistent plugin data under this directory unless another manifest-declared path is explicitly granted.

`STRONGHOLD_API_SOCKET`:

Host-mediated socket path for Stronghold secret reads. Sidecars MUST use this channel for secret access and MUST NOT receive raw long-lived secrets through environment variables.

Other plugin-specific config MAY be passed from the manifest. Such config MUST be non-secret unless explicitly routed through the approved secret mechanism.

## Lint Enforcement Recipe

Rust workspace linting MUST ban direct stdout and unapproved stderr printing in sidecar crates.

Recommended workspace configuration:

```toml
[workspace.lints.clippy]
print_stdout = "deny"
print_stderr = "deny"
dbg_macro = "deny"
```

Sidecar crates SHOULD opt into workspace lints:

```toml
[lints]
workspace = true
```

Approved logging MUST go through a wrapper such as:

```rust
agent_log!(level, "message"; "plugin_id" => plugin_id, "client_id" => client_id);
```

The wrapper implementation is the only place allowed to write to stderr or a log file directly. Production sidecar code MUST NOT call `println!`, `print!`, `dbg!`, or `eprintln!` directly.

Integration test scaffold:

```text
for each MVP sidecar:
  spawn sidecar with piped stdin/stdout/stderr
  attach strict MCP newline-delimited JSON-RPC parser to stdout
  send initialize
  send 1000 log-heavy MCP requests
  keep process running for 60 seconds
  assert every stdout line is valid UTF-8
  assert every stdout line is valid single-line JSON
  assert every stdout line is valid JSON-RPC request, response, or notification
  assert no embedded newline appears inside a message
  assert no parser desynchronization occurs
  assert logs appear only on stderr or in the configured log file
  send mcp/shutdown
  assert process exits cleanly
```

Any stdout parser error fails the test.

## Required DOs / DON'Ts

MUST:

- MUST expose MCP over stdio transport.
- MUST use newline-delimited UTF-8 JSON-RPC framing.
- MUST reserve stdout exclusively for MCP protocol messages.
- MUST reserve stdin exclusively for MCP protocol messages.
- MUST write logs only to stderr or the configured per-process log file.
- MUST use distinct `client_id`s for host-UI and per-claude sidecars.
- MUST run one OS process per `client_id`.
- MUST key `SidecarManager` state by `client_id`.
- MUST URL-encode `client_id` when used in log filenames.
- MUST redact tokens, email bodies, OAuth payloads, and secrets before logging.
- MUST support graceful `mcp/shutdown`.
- MUST exit cleanly on graceful shutdown when possible.
- MUST tolerate SIGTERM and SIGKILL fallback.
- MUST apply exponential restart backoff after unexpected exit.
- MUST reset restart backoff after 5 minutes of stable uptime.
- MUST cap restart attempts at approximately 20 per hour per `client_id`.
- MUST surface unrecoverable sidecar failure in the affected plugin UI.
- MUST ensure other plugins are unaffected by one sidecar crash.
- MUST remove MountRegistry entries when a sidecar terminates.

MAY:

- MAY mirror logs to stderr during development.
- MAY use external logrotate if the 10 MB truncate or rotate guarantee is still met.
- MAY skip `mcp/shutdown` if the MCP transport is already corrupt or closed.
- MAY expose a manual retry button that resets restart backoff.
- MAY add plugin-specific manifest config through environment variables when non-secret.
- MAY implement richer internal lifecycle states if externally observable behavior matches this spec.

MUST NOT:

- MUST NOT expose plugin MCP servers over SSE or HTTP for this project lifecycle.
- MUST NOT write logs, banners, debug output, panic text, or child-process output to stdout.
- MUST NOT call `println!`, `print!`, `dbg!`, or direct `eprintln!` from sidecar production code.
- MUST NOT put embedded newlines inside MCP stdio JSON-RPC messages.
- MUST NOT share one sidecar process across multiple `client_id`s.
- MUST NOT share stdio pipes across sidecars.
- MUST NOT give `host_ui:<plugin_id>` sidecars `cross_tab_read=true`.
- MUST NOT leak refresh tokens, access tokens, email bodies, OAuth payloads, or Stronghold responses into logs.
- MUST NOT leave orphan sidecar processes after normal app quit.
- MUST NOT let one plugin sidecar crash terminate unrelated plugin sidecars.
- MUST NOT continue retrying automatically after the restart budget is exceeded.
