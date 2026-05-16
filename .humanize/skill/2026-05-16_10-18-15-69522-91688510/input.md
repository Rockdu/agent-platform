# Ask Codex Input

## Question

Produce the complete content of `docs/specs/plugin-contract.md` — the canonical specification for this project's plugin contract pipeline. This spec is the source of truth that all downstream coding tasks (task3 codegen, task4 dispatcher, task5 SQLite framework, task6 Stronghold, task7 frontend lifecycle hooks, task10 MCP stdio helper, task11 sidecar lifecycle, task13 claude wiring, task20 orchestrator tab, task25 confirm-on-write queue) will consult.

# Project Context (do NOT propose alternatives — these are locked decisions from `.humanize/plans/plan.md`)

Stack: Tauri v2 + Rust + React/TS desktop app, macOS-first. Plugins are OS-process-isolated MCP-server sidecars (stdio transport). Each `claude` instance fork-execs its own copy of each plugin sidecar (N×M model). Single privileged orchestrator tab top-left fixed. Per-tab workspaces at `~/AgentPlatform/workspaces/<name>/`. Confirm-on-write modal gates every plugin-mediated write. Plaintext SQLite + Stronghold for OAuth tokens.

Target ACs that this spec must enable:
- AC-1.1: plugin.toml manifest schema enforced (name, version, type, command_bin, frontend, permissions, requiredApis, dbNamespace, migrations_path). Manifest uniqueness checks (no duplicate plugin_id / db_namespace / command_bin).
- AC-1.4: mandatory permission gating at Rust IPC dispatcher.
- AC-1.5: branded-handle PluginCapability lifecycle (opaque, non-serializable, non-globally-exposed, held in React component closure, bound to (plugin_id, mount_id) at MountRegistry, rotates on unmount/remount).

# Required Output

Write a complete spec document (target 200-400 lines) covering AT MINIMUM these sections (use these exact `##` headings):

## Overview
What the plugin contract is, who reads it, what it constrains.

## Manifest Schema (`plugin.toml`)
- Complete TOML schema with EVERY required field + types + validation rules
- Optional fields (with defaults)
- Uniqueness invariants (plugin_id, db_namespace, command_bin)
- Build-time validation rules + error format
- Example manifest for a hypothetical "notes" plugin

## Plugin Directory Layout
- File/directory structure under `plugins/<id>/` (manifest, Rust adapter crate, frontend dir)
- Naming conventions

## IPC Command/Event Schema (Rust source of truth)
- Rust attribute macro form for declaring commands (`#[plugin_command(name = "...", permissions = ["..."])]`)
- Generated artifact paths (Rust registry, frontend TS wrappers, per-command permission metadata, frontend tab registry)
- Single-source-of-truth principle: how dispatcher gating + TS wrapper docstring + frontend tab registry all flow from the same Rust annotations
- Error type taxonomy (PermissionDenied, CapabilityInvalid, CapabilityExpired, CapabilityMismatched, CapabilityMissing, etc.)

## PluginCapability Lifecycle
- Definition: opaque branded handle (server-issued nonce, e.g. 128-bit random token)
- Where it lives: held only in plugin's React component closure; NEVER on `window`, in `localStorage`/`sessionStorage`/`IndexedDB`, or in user-facing serialized JSON
- Authoritative state: Rust `MountRegistry` keyed by `(plugin_id, mount_id)` storing `{handle_nonce, permissions, cross_tab_read_flag (orchestrator only)}`
- Issuance: at component mount via `usePluginCapability` hook → host generates nonce + registers mount → returns handle to React via Tauri IPC
- Rotation: on unmount AND on remount (old nonce invalidated; new nonce issued); MountRegistry cleans expired entries
- Validation: dispatcher looks up handle on every permissioned IPC; rejects with typed error matching the failure mode
- Race safety: in-flight IPC during rotation either completes atomically against pre-rotation MountRegistry snapshot OR rejects with CapabilityExpired (no half-states)

## Permission Gating (Mandatory)
- Enumerated permission strings (e.g. `user.email`, `notify`, `cross_tab_read`, `pty.spawn`, `pty.read_scrollback`, `pty.write_stdin`, etc. — propose the MVP set)
- Manifest declares; dispatcher enforces at IPC boundary
- NO warning-only mode permitted (lower-bound enforcement requirement)
- Structured error: `PermissionDenied { caller_plugin_id, missing_permission, requested_command }`
- Per-command permission metadata generated from `#[plugin_command(permissions = [...])]` annotation; same metadata feeds dispatcher + TS wrapper docstring (single source)

## Per-Plugin SQLite Database
- File path: `${APP_DATA}/plugins/<id>/state.sqlite`
- WAL mode + `busy_timeout=5000` (rationale: multi-process sibling sidecar access under N×M)
- Migrations: advisory file-lock guarded (`state.sqlite.migration-lock`) so only one sibling sidecar copy runs migrations; others wait then resume against migrated schema
- No cross-plugin SQL exposed (host provides no API to open another plugin's file)
- Plain-text (not encrypted) — secrets live in Stronghold, NOT here

## Stronghold Secret Storage
- Refresh tokens at `(plugin_id, account_id)` keys
- Master-password setup with resume/reset via marker file `${APP_DATA}/stronghold-state/setup.marker`
- Access tokens stay in process memory (NOT persisted)
- Removal: token deletion + SQLite metadata cleanup at account-removal event

## Lifecycle Hooks (Frontend)
- React component contract: `onMount` (issue capability), `onUnmount` (rotate capability), `onError` (host error boundary catches; renders plugin's own error state)
- Required UI states each plugin must implement: empty-state, loading-state, error-state

## Sidecar Identity (host-UI vs per-claude)
- `client_id` scheme: `host_ui:<plugin_id>` vs `claude:<tab_id>:<plugin_id>`
- Distinct log file path per client_id: `${APP_DATA}/logs/<plugin_id>/<client_id>.log`
- Distinct MountRegistry entries (orchestrator's mount may have `cross_tab_read=true`; regular tabs never)

## Code-gen Pipeline (build.rs Output Contract)
- Scans `plugins/*/plugin.toml`
- Emits to gitignored dirs:
  - `src-tauri/src/generated/plugin_registry.rs` — Rust dispatcher registry
  - `src/generated/plugin-tabs.ts` — Frontend tab registry with lazy imports
  - `src/generated/plugins/<id>.ts` — Per-plugin typed TS wrappers
- Clean-on-build semantics (no stale)
- Validation: manifest schema + uniqueness checks; build fails with typed error naming offending plugin

## Threat Model
- MVP single Tauri WebView; plugin isolation rests on capability not being globally exposed + Rust-side dispatcher binding by (plugin_id, mount_id)
- OS process boundary is the only sandbox (no App Sandbox, no sandbox-exec for MVP)
- Plugin-vs-malicious-content (e.g., HTML in Gmail body) requires plugin to render untrusted content in sandboxed sub-frame (plugin-author responsibility)
- Per-plugin WebViews + tighter sandbox = Phase-2 hardening

## Normative DOs and DON'Ts
- A bulleted list of what plugin authors MUST do, MAY do, and MUST NOT do (raw `invoke('plugin.…')` strings, sharing capability via global, etc.)

# Output Format

Output ONLY the markdown content of the spec file. No preamble like "Here is the spec:". Start directly with `# Plugin Contract Specification` as the document title. Do NOT include any meta-commentary about the prompt or the process. The output will be saved verbatim to `docs/specs/plugin-contract.md`.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_10-18-15
- Tool: codex
