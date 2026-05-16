# Agent Platform

A Tauri v2 desktop app for personal information + agent terminal management.

**Current state**: planning phase. Architecture locked via 6 rounds of clarification on 2026-05-16; MVP implementation begins under the RLCR loop.

- Architecture & MVP plan: [`docs/plan.md`](docs/plan.md)
- Plugin specs (produced by Codex during Round 2): [`docs/specs/`](docs/specs/)
- Idea draft (v2, local-only — `.humanize/` is gitignored per humanize plugin policy): `.humanize/ideas/idea-2026-05-16-v2.md`

## High-level shape

- Stack: Tauri v2 + Rust + React/TypeScript desktop app, macOS-first.
- Plugins are OS-process-isolated MCP-server sidecars (stdio transport). Each `claude` instance forks its own copy of every plugin sidecar.
- Single privileged orchestrator tab (top-left, single-instance) auto-launches `claude` with preconfigured MCP servers.
- Three MVP plugin tabs: **Terminal Mesh** (per-workspace PTYs), **Gmail** (multi-account), **Papers** (Zotero + arXiv).
- Workspaces are user-visible dirs under `~/AgentPlatform/workspaces/<name>/` — accessible by Cursor / VS Code for code review.
- Confirm-on-write modal gates every plugin-mediated write from `claude` (per-message, host-arbitrated queue).
- Notification surface: native macOS notification + menubar tray window (`tauri-plugin-positioner` TrayBottomCenter) — the desktop analog of iOS Dynamic Island.
- UI is Chinese-first for MVP; English deferred to Phase 2.

## Phase 2 (explicitly deferred from MVP)

WeChat, GitHub PR review, user-installable plugins, encrypted SQLite, personalized arXiv ranking, `gmail.delete` scope, iOS companion with true Live Activities, App Sandbox tighter than OS process boundary, per-plugin sidecar dedup proxy.
