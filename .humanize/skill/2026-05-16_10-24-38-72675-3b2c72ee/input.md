# Ask Codex Input

## Question

Produce the complete content of `docs/specs/mcp-sidecar.md` — the canonical specification for MCP stdio sidecar discipline + lifecycle in this project. Downstream consumers: task10 (MCP stdio framing helper crate), task11 (sidecar lifecycle manager), task28/task34 (Gmail/Papers sidecar scaffolds).

# Locked context (do NOT propose alternatives)

Plugins are OS-process-isolated sidecar binaries. Each plugin exposes an MCP server over **stdio transport** (not SSE, not HTTP). The Tauri host owns its UI sidecar set; each `claude` tab fork-execs its own per-claude sidecar set (N×M model). Auto-restart on crash with exponential backoff (1s → 2s → 4s → ... → 60s upper cap; reset after 5 minutes of stable uptime). Graceful Shutdown signal with 5-second timeout before SIGKILL. Distinct `client_id` per sidecar: `host_ui:<plugin_id>` vs `claude:<tab_id>:<plugin_id>`. Logs go to stderr or per-process log files; NEVER stdout (stdout carries MCP protocol bytes only).

Target ACs:
- **AC-2.1**: MCP stdio framing correctness; stdout protocol-only; logs to stderr/files; lint rule banning `println!`/`console.log` in sidecar code.
- **AC-1.6**: Distinct host-UI vs per-claude client identities; separate `client_id`s, log prefixes, capability scopes; graceful Shutdown contract; 5-second SIGKILL timeout.

# Required Output

Spec sections (use these exact `##` headings):

## Overview
What this spec defines, who reads it, what it constrains.

## Transport: MCP Stdio
- Why stdio (matches `claude` CLI default; no port allocation; no authentication overhead; fork-exec lifecycle).
- Framing: how MCP message framing works over stdio (length-prefixed JSON-RPC vs newline-delimited — pick one per MCP 2025-11-25 spec and cite).
- stdin/stdout are protocol channels; stderr is the log channel; any out-of-band file IO uses per-process log files at `${APP_DATA}/logs/<plugin_id>/<client_id>.log`.

## Stdout Discipline
- ABSOLUTE rule: sidecar code may NEVER write to stdout outside the MCP framing layer.
- Enforcement mechanisms: lint rule in Rust (deny `println!`, `print!`, `dbg!`, `eprintln!` macros except via approved logging wrappers); Cargo workspace lints. Integration test (pipe sidecar stdout through strict MCP parser during a 60-second log-heavy test run; zero framing errors).
- If sidecar inadvertently writes to stdout, the host MCP client will see desync — describe the failure mode and recovery (restart sidecar, log incident).

## Log Channel
- Default sink: per-process file at `${APP_DATA}/logs/<plugin_id>/<client_id>.log` (urlencode `client_id` for filename safety; the colon in `host_ui:notes` becomes `%3A`).
- stderr is also acceptable for dev/debugging.
- Structured JSON logs with correlation IDs (`tab_id`, `plugin_id`, `client_id`, `request_id`).
- Redaction: refresh tokens, access tokens, email bodies, OAuth payloads MUST be redacted before emission.
- Log rotation: file-size-based or external logrotate; spec mandates minimum behavior (truncate at 10MB or rotate to `.log.1`).

## Sidecar Identity Scheme
- `client_id` format: `host_ui:<plugin_id>` for host-owned UI sidecars; `claude:<tab_id>:<plugin_id>` for per-claude tab sidecars. Tab IDs are UUIDv4 strings.
- One sidecar PROCESS per `client_id`.
- The host's `SidecarManager` keys by `client_id`.
- Different `client_id`s = different OS processes = different stdio pipes = different log files = different MountRegistry entries.

## Lifecycle Manager Contract
- Spawn: `SidecarManager::spawn(plugin_id, client_id, env_vars)` fork-execs the manifest-declared `command_bin`, opens stdio MCP transport, registers the process with its PID.
- Shutdown signal: host sends `mcp/shutdown` (or implementation-specific equivalent — propose a graceful protocol).
- Grace period: 5 seconds from Shutdown send to SIGKILL fallback.
- Auto-restart: on unexpected exit (non-zero code or signal), restart with exponential backoff (1s, 2s, 4s, 8s, 16s, 32s, 60s cap); reset backoff counter after 5 minutes of stable uptime.
- Restart budget: cap at ~20 restarts in any 1-hour window; beyond that, surface unrecoverable error state and stop retrying until manual retry.
- Crash UI: affected plugin's tab shows error state with manual retry button; other plugins unaffected.

## Shutdown Sequence (per process)
1. Host sends `mcp/shutdown` request via stdio.
2. Sidecar flushes in-flight state (pending writes, log buffers).
3. Sidecar closes its MCP transport.
4. Sidecar process exits cleanly (exit code 0).
5. If sidecar does NOT exit within 5 seconds: host sends SIGTERM.
6. If still alive after another 2 seconds: SIGKILL.
7. Host marks the sidecar as terminated in `SidecarManager` and removes its MountRegistry entries.

## Host App Quit Sequence (vs single-sidecar Shutdown)
- AppQuitCoordinator orchestrates termination of all sidecars (host-UI + every active per-claude).
- Order: stop accepting new IPC writes → resolve/cancel pending confirm-on-write modals → terminate PTYs (per Terminal Mesh spec) → graceful Shutdown to all sidecars → 5-second wait → SIGKILL stragglers.
- No orphan processes after normal quit. On startup, scan `${APP_DATA}/sidecar-state/` for stale PID records from crashed/force-quit prior runs and reap them where possible.

## Sidecar Crash Recovery
- Detect: SidecarManager monitors process via Tokio task; on exit (signal or non-zero code), trigger restart logic.
- Backoff: 1s, 2s, 4s, 8s, 16s, 32s, 60s, 60s, 60s, ... (cap at 60s).
- Reset criterion: 5 minutes of stable uptime without restart.
- During restart: plugin's tab UI renders error state ("Plugin <name> restarting in <next_backoff>s, attempt N. [Retry now]") with manual retry button that immediately attempts spawn (resets backoff).
- Orchestrator's claude sees disconnected MCP server during restart; the next MCP request fails with a typed error; claude can surface it to user but does NOT crash.

## Distinct Identities: host-UI vs per-claude
- Why distinct: host UI may need long-lived authoritative sidecar (e.g., Gmail inbox view) while per-claude sidecars are ephemeral; different log lifetimes, different MountRegistry scopes, different capability sets (host_ui never has `cross_tab_read=true`; orchestrator's claude:<tab_id>:terminal_mesh CAN).
- SidecarManager maintains a `HashMap<client_id, ProcessHandle>` and per-`client_id` lifecycle FSM.

## Env Vars Passed to Sidecar
- `PLUGIN_ID`: which plugin.
- `CLIENT_ID`: full client identifier.
- `TAB_ID`: present only for per-claude (`claude:<tab_id>:...`).
- `WORKSPACE_PATH`: present only for per-claude (the bound workspace dir).
- `APP_DATA_DIR`: writable per-plugin data dir.
- `STRONGHOLD_API_SOCKET`: how to reach Stronghold for secret reads (host-mediated).
- Other plugin-specific config from manifest.

## Lint Enforcement Recipe
- Rust: cargo workspace `[lints]` section banning `print_macros` (clippy lint `clippy::print_stdout`, `clippy::print_stderr` selectively). Approved wrapper: `agent_log!(level, "..." )` macro that always routes to the log channel.
- Integration test scaffold (pseudocode): spawn each MVP sidecar, send 1000 log-heavy MCP requests, pipe stdout through strict parser; fail if any deserialization error.

## Required DOs / DON'Ts
- MUST/MAY/MUST NOT bulleted contract for sidecar authors.

# Output Format

Output ONLY the markdown spec content. Start with `# MCP Sidecar Discipline Specification`. No preamble. Saved verbatim to `docs/specs/mcp-sidecar.md`.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_10-24-38
- Tool: codex
