# Ask Codex Input

## Question

Produce the complete content of `docs/specs/claude-launch.md` — the canonical spec for launching `claude` CLI from the Tauri host with proper PATH discovery + per-tab MCP config generation. Downstream consumer: task13 (claude PATH discovery + per-tab MCP config generator + onboarding card if missing).

# Locked context

The Tauri host is a macOS GUI app launched by Finder/Dock, which does NOT inherit the user's interactive shell PATH. `claude` CLI is typically installed via brew (`/opt/homebrew/bin/claude`) or npm-global or manually. The host MUST probe for `claude` itself and persist the discovered path. Each Terminal Mesh tab that launches `claude` MUST generate a per-tab MCP config file listing exactly one sidecar invocation per plugin and pass it to `claude` via the documented CLI flag. The orchestrator tab auto-launches `claude` on open with the platform's full MCP server set preconfigured; regular Terminal Mesh tabs do NOT auto-launch claude (the user types `claude` themselves if they want it — but when they do, MCP is still available via the same config mechanism if the tab spawns).

Target ACs:
- **AC-2.2**: Per-tab MCP config generator; cleanup on tab close + startup GC.
- **AC-2.3**: `claude` PATH discovery NOT relying on GUI app shell inheritance.
- **AC-3.2**: Orchestrator auto-launches `claude` on open; onboarding card if `claude` is unavailable.

# Required Output

Sections (use exact `##` headings):

## Overview

## PATH Discovery Algorithm
- First-launch probe sequence:
  1. Check user-saved `claude_path` in `${APP_DATA}/claude-config.json` (if previously discovered).
  2. Probe known install paths in order: `/opt/homebrew/bin/claude`, `/usr/local/bin/claude`, `~/.local/bin/claude`, `~/.npm-global/bin/claude`.
  3. Run `bash -lc 'command -v claude'` to get the user's login-shell PATH-resolved claude (this works because bash -lc sources `.zprofile`/`.bash_profile`).
  4. If all fail: emit `ClaudeNotFound` typed error; orchestrator onboarding card surfaces a file picker.
- Persistence: discovered path written to `${APP_DATA}/claude-config.json` with `{path: "...", version: "...", discovered_at: "..."}`. Re-validated on every app start (existsAndExecutable check).
- User override: settings UI exposes "Choose claude binary..." file picker that writes to the same config file.

## Minimum Supported Claude Version
- Pin a minimum version (propose one based on MCP support — claude code with stdio MCP config support).
- On version mismatch, surface upgrade nudge in orchestrator UI; do NOT block launch (newer versions OK; older may degrade MCP features).
- How version is detected: `claude --version` parsing.

## MCP Config Generation (per Terminal Mesh tab)
- Path convention: `${APP_DATA}/claude-mcp-configs/<tab_id>.json`.
- Content schema:
  ```json
  {
    "mcpServers": {
      "<plugin_id>": {
        "command": "/path/to/<plugin_id>-sidecar",
        "args": ["--client-id", "claude:<tab_id>:<plugin_id>", "--workspace", "<workspace_path>"],
        "env": { "APP_DATA_DIR": "..." }
      }
    }
  }
  ```
- One MCP server entry per discovered plugin sidecar binary.
- `args` passes the per-claude client_id and the workspace_path so the sidecar knows its context.
- `env` propagates required env vars.

## Tab → Claude Launch Flow
1. User opens Terminal Mesh tab; workspace is bound; PTY is allocated; default shell starts.
2. Orchestrator-tab variant: host auto-types `claude` (or invokes equivalent CLI) on PTY ready.
3. Before `claude` starts: host generates the per-tab MCP config (write to `${APP_DATA}/claude-mcp-configs/<tab_id>.json`).
4. Host invokes `claude --mcp-config <config_path>` (exact flag name per claude CLI documentation; verify the current flag).
5. Sidecar processes are fork-exec'd by claude on first tool call (lazy) per MCP stdio semantics.

## Cleanup
- On tab close: host deletes `${APP_DATA}/claude-mcp-configs/<tab_id>.json`. Any sidecars claude spawned are claude's responsibility (it should terminate them on its own exit).
- Startup GC: host scans `${APP_DATA}/claude-mcp-configs/` at startup; any file older than the host's previous PID lifetime is unlinked.

## Orchestrator Tab Specifics
- Single instance enforced by `OrchestratorLock` (singleton in host state); opening "Orchestrator" while one exists focuses the existing tab.
- Auto-launches `claude` on tab open with MCP config preconfigured (one sidecar per MVP plugin: Terminal Mesh, Gmail, Papers).
- Distinct icon and fixed top-left position in tab strip.
- Onboarding card path (when `claude` is missing): renders a Chinese onboarding card listing:
  - The paths probed (so user can verify why discovery failed).
  - A "选择 claude 路径" file picker.
  - Install hint: `brew install claude` or link to Anthropic install docs.
  - "Retry discovery" button.
- Onboarding card does NOT fork-exec any plugin sidecars (per AC-2.3 negative test).

## Orchestrator's Special MCP Config
- The orchestrator's MCP config differs from regular tab configs in one way: the Terminal Mesh sidecar entry includes `--cross-tab-read` arg (or equivalent capability signal) so its sidecar knows this client is privileged. The HOST also separately registers a `cross_tab_read=true` MountRegistry flag for this mount — the arg is informational; actual enforcement is dispatcher-side.

## Failure Modes & UX
- `claude --version` fails to parse → assume unsupported version; offer upgrade nudge but allow launch.
- `claude` binary moves/disappears between launches → re-run PATH discovery; persist new path.
- MCP config write fails (disk full / permission) → typed `McpConfigWriteFailed` error; tab refuses to launch claude; surface in Chinese error card.
- Per-tab sidecar fork-exec fails → claude reports the error via MCP framing; tab shows it.

## Security Considerations
- MCP config file at `${APP_DATA}/claude-mcp-configs/<tab_id>.json` does NOT contain any secrets (no tokens, no passwords). The `env` block may include `APP_DATA_DIR` and other non-secret config.
- Stronghold is reached by sidecars at runtime via a separate channel, not via the MCP config.
- The MCP config file is readable by other processes under the user account; this is acceptable because the file contains no secrets.

## DOs / DON'Ts

# Output Format

Output ONLY the markdown spec. Start with `# Claude CLI Launch Specification`. No preamble. Saved verbatim to `docs/specs/claude-launch.md`.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_10-26-16
- Tool: codex
