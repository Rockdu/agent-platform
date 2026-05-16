# Claude CLI Launch Specification

## Overview

This specification defines how the Tauri host discovers and launches the `claude` CLI from a macOS GUI app, and how it generates per-tab MCP configuration for Terminal Mesh tabs.

The host is launched by Finder/Dock and therefore must not rely on inherited interactive shell `PATH`. It must discover the `claude` binary itself, persist the discovered path, re-validate it on startup, and provide a user-facing recovery path when discovery fails.

Each Terminal Mesh tab that launches `claude` must receive a per-tab MCP config file containing exactly one sidecar invocation per discovered plugin. The orchestrator tab auto-launches `claude` with the full platform MCP server set. Regular Terminal Mesh tabs do not auto-launch `claude`; users type `claude` themselves, but the host must still make the same MCP config mechanism available when the tab spawns `claude`.

This spec targets:

- **AC-2.2**: Per-tab MCP config generator; cleanup on tab close and startup GC.
- **AC-2.3**: `claude` PATH discovery without relying on GUI app shell inheritance.
- **AC-3.2**: Orchestrator auto-launches `claude` on open; onboarding card if `claude` is unavailable.

## PATH Discovery Algorithm

The host owns `claude` discovery. It must not assume that the GUI app process has a useful `PATH`.

First-launch probe sequence:

1. Check the user-saved `claude_path` in `${APP_DATA}/claude-config.json`, if previously discovered.
2. Probe known install paths in this exact order:
   - `/opt/homebrew/bin/claude`
   - `/usr/local/bin/claude`
   - `~/.local/bin/claude`
   - `~/.npm-global/bin/claude`
3. Run `bash -lc 'command -v claude'` to get the user's login-shell PATH-resolved `claude`. This catches shell-configured installs that are invisible to Finder/Dock-launched apps.
4. If all probes fail, emit a typed `ClaudeNotFound` error. The orchestrator onboarding card surfaces a file picker and retry action.

A candidate path is valid only if `existsAndExecutable(path)` succeeds.

Persistence:

- On successful discovery, write `${APP_DATA}/claude-config.json`.
- Stored shape:

```json
{
  "claude_path": {
    "path": "/opt/homebrew/bin/claude",
    "version": "2.0.0",
    "discovered_at": "2026-05-16T12:00:00Z"
  }
}
```

- The stored path is re-validated on every app start with `existsAndExecutable`.
- If the stored path is missing or no longer executable, the host re-runs the full discovery sequence and persists the new result if found.
- If re-discovery fails, the host clears the active in-memory `claude` path and surfaces `ClaudeNotFound`.

User override:

- Settings UI exposes **Choose claude binary...**.
- The file picker result is validated with `existsAndExecutable`.
- On success, the host runs `claude --version`, stores the path/version/discovery timestamp in `${APP_DATA}/claude-config.json`, and uses that path for future launches.
- On validation failure, the settings UI surfaces a Chinese error message and does not persist the override.

## Minimum Supported Claude Version

Minimum supported version: **Claude Code `2.0.0`**.

Rationale:

- The host relies on documented Claude Code MCP config loading via `--mcp-config`.
- The MCP config uses stdio server entries with `command`, `args`, and `env`.
- Older versions may still launch, but MCP config behavior may be incomplete or incompatible.

Version detection:

- Run:

```bash
claude --version
```

- Parse the first semantic version-like token matching:

```text
\d+\.\d+\.\d+
```

- Examples:
  - `Claude Code 2.0.1` → `2.0.1`
  - `claude 2.1.128` → `2.1.128`

Version behavior:

- Version `>= 2.0.0`: supported.
- Version `< 2.0.0`: surface an upgrade nudge in the orchestrator UI, but do not block launch.
- Version parse failure: assume unsupported version, surface upgrade nudge, but allow launch.
- Newer versions are accepted.

Upgrade nudge copy should be Chinese and non-blocking, for example:

```text
检测到的 Claude Code 版本可能不完整支持 MCP 配置。建议升级 Claude Code，以获得完整插件能力。
```

## MCP Config Generation (per Terminal Mesh tab)

Path convention:

```text
${APP_DATA}/claude-mcp-configs/<tab_id>.json
```

The host creates the parent directory if missing.

Content schema:

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

Rules:

- Generate exactly one MCP server entry per discovered plugin sidecar binary.
- `plugin_id` must be stable and unique across enabled plugins.
- `command` must be the absolute path to that plugin's sidecar binary.
- `args` must include:
  - `--client-id`
  - `claude:<tab_id>:<plugin_id>`
  - `--workspace`
  - `<workspace_path>`
- The per-Claude `client_id` lets the sidecar distinguish clients and correlate mount/session context.
- The workspace path tells the sidecar which workspace this tab is bound to.
- `env` propagates required non-secret environment variables, including at minimum:
  - `APP_DATA_DIR`
- The MCP config must not include tokens, passwords, OAuth credentials, API keys, or Stronghold material.
- Config JSON must be written atomically:
  - write to a temporary file in `${APP_DATA}/claude-mcp-configs/`
  - `fsync` if available
  - rename into `<tab_id>.json`

The host should keep an in-memory mapping:

```text
tab_id -> mcp_config_path
```

This mapping is used for launch, cleanup, and diagnostics.

## Tab → Claude Launch Flow

1. User opens a Terminal Mesh tab; workspace is bound; PTY is allocated; default shell starts.
2. Orchestrator-tab variant: host auto-types `claude` or invokes the equivalent CLI on PTY ready.
3. Before `claude` starts, host generates the per-tab MCP config at `${APP_DATA}/claude-mcp-configs/<tab_id>.json`.
4. Host invokes Claude Code with the documented MCP config flag:

```bash
/path/to/claude --mcp-config /path/to/<tab_id>.json
```

The current documented flag is `--mcp-config`. Claude Code also documents `--strict-mcp-config`, which may be used when the host must ensure only the generated per-tab config is loaded:

```bash
/path/to/claude --strict-mcp-config --mcp-config /path/to/<tab_id>.json
```

For Terminal Mesh, the preferred launch form is:

```bash
/path/to/claude --strict-mcp-config --mcp-config "${APP_DATA}/claude-mcp-configs/<tab_id>.json"
```

This ensures the tab receives exactly the sidecar set generated by the host and does not accidentally inherit user/global/project MCP servers.

5. Sidecar processes are fork-exec'd by `claude` on first tool call, per MCP stdio semantics.

Regular Terminal Mesh tabs:

- Do not auto-launch `claude`.
- The default shell starts normally.
- When the tab spawns `claude` through the host-controlled launch path, the host injects the same `--strict-mcp-config --mcp-config <config_path>` arguments.
- If the user manually runs some other `claude` binary directly inside the shell, the host cannot guarantee MCP injection unless command mediation is implemented for that shell session.

Orchestrator tab:

- Always launches through the host-controlled path.
- Must generate the MCP config before launch.
- Must use the persisted/discovered absolute `claude` path.

## Cleanup

On tab close:

- Host deletes:

```text
${APP_DATA}/claude-mcp-configs/<tab_id>.json
```

- Missing file is not an error.
- Deletion failure is logged and surfaced only in diagnostics; it should not block tab close.
- Any sidecars spawned by `claude` are `claude`'s responsibility. The host should not independently kill sidecars unless future process supervision explicitly owns them.

Startup GC:

- On startup, the host scans:

```text
${APP_DATA}/claude-mcp-configs/
```

- The host unlinks stale config files left by previous app runs.
- A file is stale if it is older than the host's previous PID lifetime or cannot be associated with a currently live tab/session.
- Startup GC must run before any new tab config is generated.
- GC must only delete files under `${APP_DATA}/claude-mcp-configs/`.

Recommended metadata:

- Store a lightweight host runtime marker:

```text
${APP_DATA}/host-runtime.json
```

- Include:
  - previous host PID
  - previous startup time
  - previous shutdown time, if clean shutdown occurred

If no reliable previous PID lifetime data exists, startup GC may conservatively delete all existing files in `${APP_DATA}/claude-mcp-configs/`, because these files are ephemeral and contain no secrets.

## Orchestrator Tab Specifics

Single instance:

- Enforced by `OrchestratorLock`, a singleton in host state.
- Opening **Orchestrator** while one exists focuses the existing tab.
- Closing the orchestrator releases the lock.

Placement and identity:

- Distinct icon.
- Fixed top-left position in the tab strip.
- Cannot be reordered behind regular Terminal Mesh tabs.

Launch behavior:

- Auto-launches `claude` on tab open after PTY readiness.
- Uses the host-discovered absolute `claude` path.
- Generates MCP config before launch.
- Launches with the platform's full MVP MCP server set preconfigured:
  - Terminal Mesh
  - Gmail
  - Papers

Onboarding card path when `claude` is missing:

- Render a Chinese onboarding card.
- Do not fork-exec any plugin sidecars.
- Do not generate sidecar processes.
- Do not attempt partial orchestrator launch.

Required onboarding card content:

```text
未找到 Claude Code

应用无法在常见位置找到 claude 命令。由于 macOS 图形界面应用不会继承终端里的 PATH，需要手动确认 Claude Code 的安装位置。

已检查路径：
/opt/homebrew/bin/claude
/usr/local/bin/claude
~/.local/bin/claude
~/.npm-global/bin/claude
bash -lc 'command -v claude'

你可以选择 claude 可执行文件，或安装 Claude Code 后重试。
```

Required actions:

- **选择 claude 路径**: opens a file picker.
- **Retry discovery**: re-runs the PATH discovery algorithm.
- Install hint:

```text
安装提示：brew install claude
```

- Include a link to Anthropic Claude Code install documentation.

Negative requirement:

- The onboarding card must not launch `claude`.
- The onboarding card must not fork-exec plugin sidecars.
- The onboarding card must not create MCP config files unless the user successfully resolves `claude` and retries launch.

## Orchestrator's Special MCP Config

The orchestrator's MCP config differs from regular tab configs in exactly one way:

- The Terminal Mesh sidecar entry includes an additional capability signal argument:

```json
"--cross-tab-read"
```

Example:

```json
{
  "mcpServers": {
    "terminal-mesh": {
      "command": "/path/to/terminal-mesh-sidecar",
      "args": [
        "--client-id",
        "claude:<orchestrator_tab_id>:terminal-mesh",
        "--workspace",
        "<workspace_path>",
        "--cross-tab-read"
      ],
      "env": {
        "APP_DATA_DIR": "..."
      }
    },
    "gmail": {
      "command": "/path/to/gmail-sidecar",
      "args": ["--client-id", "claude:<orchestrator_tab_id>:gmail", "--workspace", "<workspace_path>"],
      "env": {
        "APP_DATA_DIR": "..."
      }
    },
    "papers": {
      "command": "/path/to/papers-sidecar",
      "args": ["--client-id", "claude:<orchestrator_tab_id>:papers", "--workspace", "<workspace_path>"],
      "env": {
        "APP_DATA_DIR": "..."
      }
    }
  }
}
```

The `--cross-tab-read` argument is informational. Actual authorization is enforced dispatcher-side.

The host must also register a matching MountRegistry flag for the orchestrator mount:

```text
cross_tab_read=true
```

Enforcement rules:

- Sidecars may use the argument to select behavior or diagnostics.
- Sidecars must not treat the argument alone as authority.
- Dispatcher/MountRegistry authorization is the source of truth.

## Failure Modes & UX

`claude --version` fails to parse:

- Treat as unsupported/unknown version.
- Surface non-blocking Chinese upgrade nudge.
- Allow launch.

`claude` binary moves or disappears between launches:

- Startup re-validation fails.
- Host re-runs PATH discovery.
- If a new path is found, persist it.
- If no path is found, emit `ClaudeNotFound` and show orchestrator onboarding card.

MCP config write fails:

- Emit typed `McpConfigWriteFailed`.
- Tab refuses to launch `claude`.
- Surface a Chinese error card.

Suggested Chinese copy:

```text
无法启动 Claude Code

应用未能写入本标签页的 MCP 配置文件。请检查磁盘空间和应用数据目录权限后重试。
```

Include diagnostic detail:

- target config path
- OS error code/message
- tab id
- workspace path

Per-tab sidecar fork-exec fails:

- `claude` reports the error through MCP framing.
- Tab shows the error in the Claude session output.
- Host does not retry sidecar execution independently.

Invalid sidecar path:

- The host excludes missing or non-executable sidecar binaries from config generation only if the plugin is optional.
- For required MVP plugins in the orchestrator, missing sidecars should surface a Chinese degraded-capability warning before launch.
- The warning does not block launch unless no usable MCP server entries can be generated.

Invalid workspace path:

- Emit typed `WorkspaceUnavailable`.
- Do not generate MCP config.
- Do not launch `claude`.

JSON serialization failure:

- Emit typed `McpConfigWriteFailed`.
- Do not launch `claude`.

Config cleanup failure:

- Log to diagnostics.
- Do not block tab close.

## Security Considerations

MCP config files at:

```text
${APP_DATA}/claude-mcp-configs/<tab_id>.json
```

must not contain secrets.

Allowed config content:

- sidecar executable paths
- plugin IDs
- tab IDs
- workspace paths
- non-secret environment values such as `APP_DATA_DIR`
- capability hints such as `--cross-tab-read`

Forbidden config content:

- OAuth tokens
- API keys
- passwords
- Stronghold keys
- session cookies
- refresh tokens
- user email credentials
- private document contents

Stronghold access:

- Stronghold is reached by sidecars at runtime through a separate channel.
- Stronghold material is never serialized into the MCP config file.
- MCP config only tells `claude` how to start sidecars.

File visibility:

- MCP config files are readable by other processes under the same user account.
- This is acceptable because they contain no secrets.
- File permissions should still be owner-readable/writable where practical.

Recommended file mode:

```text
0600
```

Process trust:

- The host must only write sidecar commands for plugin sidecar binaries discovered from trusted application/plugin installation locations.
- User-provided plugin sidecars require the existing plugin trust/install flow.
- The orchestrator privilege flag is not an authorization boundary.

## DOs / DON'Ts

DO:

- Do discover `claude` from the host; do not depend on Finder/Dock `PATH`.
- Do persist the discovered path in `${APP_DATA}/claude-config.json`.
- Do re-validate the persisted path on every app start.
- Do provide a settings file picker for manual override.
- Do generate one per-tab MCP config file before launching `claude`.
- Do use the documented `--mcp-config` flag.
- Do prefer `--strict-mcp-config --mcp-config <path>` when the tab must receive exactly the host-generated MCP server set.
- Do write MCP config JSON atomically.
- Do clean up per-tab config files on tab close.
- Do run startup GC for stale config files.
- Do surface typed errors with Chinese user-facing cards.
- Do keep MCP config files secret-free.
- Do register orchestrator cross-tab privileges in MountRegistry, not only in CLI args.
- Do allow launch on unknown or older `claude` versions with a warning.

DON'T:

- Don't assume the Tauri host inherited the user's terminal `PATH`.
- Don't shell out to plain `claude` without first resolving an absolute executable path.
- Don't store tokens, passwords, OAuth credentials, or Stronghold material in MCP config files.
- Don't launch plugin sidecars from the onboarding card.
- Don't auto-launch `claude` in regular Terminal Mesh tabs.
- Don't rely on `--cross-tab-read` as an authorization boundary.
- Don't block `claude` launch solely because version parsing failed.
- Don't leave per-tab MCP config files around after tab close when deletion is possible.
- Don't delete files outside `${APP_DATA}/claude-mcp-configs/` during startup GC.
- Don't silently fall back to missing MCP config; if config generation fails, refuse to launch `claude` and show `McpConfigWriteFailed`.
