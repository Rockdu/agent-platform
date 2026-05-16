# Ask Codex Input

## Question

You are doing a FIRST-PASS PLANNING ANALYSIS of a greenfield Tauri v2 desktop app, BEFORE Claude synthesizes the candidate plan. Your role is to surface what's MISSING / UNDERSPECIFIED / RISKY at the IMPLEMENTATION level — NOT to revisit architectural decisions that the user has already locked.

# CRITICAL ROLE CONSTRAINT — READ BEFORE EVERYTHING ELSE

The draft below has a section titled **"Locked Decisions"** containing ~50 design decisions explicitly confirmed by the user across 6 rounds of clarification. Examples: process isolation via Tauri sidecar (no App Sandbox), MCP stdio mandatory, N×M sidecar lifecycle, single privileged orchestrator, per-tab workspaces, no auto git init, plaintext SQLite + Stronghold for secrets, ≥4 PTYs HARD, 1MB ring HARD, ≥2 Gmail accounts HARD, `gmail.modify + gmail.send` scopes, confirm-on-write modal, etc.

**DO NOT:**
- Propose alternatives to any Locked Decision
- Put any Locked Decision into `QUESTIONS_FOR_USER`
- Argue that "the user might want X instead" for any locked item
- Question whether OS-process-only sandboxing is enough (user explicitly chose this over App Sandbox)
- Question whether N×M sidecars is wasteful (user explicitly accepted the cost)
- Question whether `.claude/` is sufficient for review (user explicitly chose this over outer git)
- Question whether single orchestrator is enough (user explicitly chose this over multi-instance)

**DO:**
- Surface implementation-level gaps: build orchestration, codegen pipeline mechanics, MCP Rust SDK choice details, dispatcher dispatch logic, error UX flows, race conditions in capability rotation, etc.
- Identify edge cases NOT addressed in the draft (e.g., what happens if Stronghold master password setup is interrupted mid-flow? what if `~/AgentPlatform/` doesn't exist on first launch?)
- Suggest acceptance criteria that test the Locked Decisions (e.g., "AC: sidecar crash auto-restart with exponential backoff measurable from logs")
- Suggest task-breakdown shape based on the draft's milestones

# Repository Context

- Path: `/Users/rockdu/claude_workspace`. State: greenfield, `git init`'d 2026-05-15, only `.humanize/` + `.git/` present. v1 plan at `.humanize/plans/plan-v1-superseded.md` (in-process plugin model, abandoned). v2 draft at `.humanize/ideas/idea-2026-05-16-v2.md`.

# The v2 Draft Being Critiqued

```
# MCP-Federated Personal Agent Platform With Privileged Orchestrator And Per-Tab Workspaces

> **v2 — supersedes** `.humanize/ideas/idea-20260515-224824.md` (in-process plugin model) after a user-initiated architectural pivot on 2026-05-16. The pivot adds (a) OS-process isolation per plugin, (b) a privileged orchestrator terminal with cross-plugin context, (c) workspace isolation per Terminal Mesh tab. All locked decisions in this document come from explicit user confirmation across 6 question rounds; do NOT re-litigate them during gen-plan.

## Original Idea (verbatim, Chinese)

我希望你在这个工作区里面开一个新仓库，这个仓库将要作为一个agent平台，担任我所有信息管理（邮件收发（连接gmail多个账号）、消息管理（连接微信）、论文管理和推荐（连接我的zotero和arxiv））、agent终端管理（我会开多个终端，我需要能实时查看各个终端的状态，并且某个终端工作完成了或者需要我操作了给我一个灵动岛提示）；并且需要支持增加更多的tab功能（比如后期我可能需要github仓库追踪和辅助pr code review功能等）；你要考虑到工程可维护性、前端美观度以及可扩展性

**Subsequent clarifications** (also user statements):
- 一个 privileged 大终端跑 claude 拿全部平台 context；其它 plugin 进程级分离
- 每个 claude code 终端都要有工作区隔离，每个终端跟进一个问题
- 我希望能用 Cursor 访问每个 workspace 去 review agent 做的事情和写的代码

## Primary Direction: MCP-Federated Process-Isolated Plugins + Privileged Orchestrator + Per-Tab Workspaces

### Rationale

A Tauri v2 desktop app where every plugin (Gmail, Papers, Terminal Mesh, future WeChat/GitHub) runs as its own OS-process MCP-server sidecar; the host UI and the user's `claude` instances are all MCP clients of those sidecars. A privileged "orchestrator" tab at the top-left of the tab strip runs `claude` with platform-wide context and write authority (via OAuth delegation, never token leak). Regular Terminal Mesh tabs are each scoped to one workspace (a real directory under `~/AgentPlatform/workspaces/<name>/`) so each terminal cleanly tracks one problem and is reviewable by external editors like Cursor. This is distinguished from the previous in-process plugin design by combining standard-protocol federation (MCP) with strict per-plugin process boundaries and a user-pointable workspace concept that doubles as an IDE-accessible review surface.

### Approach Summary

**Tauri host (Rust)** is a thin orchestrator: spawns plugin sidecars (one per plugin, owned by the host for UI rendering), generates typed IPC wrappers, enforces permission gating, owns notification dedup and the menubar tray. The host's React frontend renders one tab per plugin plus one privileged orchestrator tab. Terminal Mesh is the plugin that hosts all PTY tabs (each tab = one workspace).

**Plugin contract** is declarative: `plugins/<id>/plugin.toml` manifest + Rust adapter crate + frontend dir. A `build.rs` script scans `plugins/` and emits the Rust registry, the typed IPC wrappers (`ts-rs`/`specta`), and the frontend tab registry (`src/generated/plugin-tabs.ts`). Adding a plugin requires zero edits to host source. Each plugin compiles to its own sidecar binary and exposes an MCP server over **stdio**.

**MCP-server-per-claude-instance lifecycle**: each `claude` running in a tab fork-execs its own copy of each plugin sidecar (N tabs × M plugins = N·M lightweight processes; accepted cost). All sidecars share state via plaintext per-plugin SQLite files (WAL multi-process read) at `${APP_DATA}/plugins/<id>/state.sqlite`; OAuth refresh tokens live only in Stronghold keyed by `(plugin_id, account_id)`. Plugin sidecar crash triggers exponential-backoff auto-restart (1s → 60s cap); affected tab shows error state with manual retry; other plugins unaffected.

**Orchestrator** (top-left fixed tab, single-instance, distinct icon, auto-launches `claude` with all platform MCP servers preconfigured): the only client that can read other tabs' scrollback (via Terminal Mesh's privileged `cross_tab_read` capability) and can perform Gmail send on the user's identity via OAuth delegation (never receives the token; signs a delegated request that the Gmail sidecar fulfills using Stronghold-held tokens). All write operations across all tabs trigger a host-level **confirm-on-write** modal — every email send, label change, Zotero edit, terminal stdin write pops a per-message confirmation.

**Workspaces** are one-to-one with Terminal Mesh tabs. Each tab is bound to a directory at `~/AgentPlatform/workspaces/<name>/` (user-visible, accessible by Cursor/VS Code/Zed) OR a user-pointed pre-existing directory like `~/repos/myproj/`. **No auto git init** — workspace is plain dir; review affordance is "Open in Cursor" + reliance on `.claude/` transcript (claude's conversation history) + inner cloned repos' own `.git`. Workspaces persist across app restart; one workspace can be open in at most one tab at a time. Workspace switcher: `+` button modal with "create new" or "pick existing"; per-row metadata = name + last used + created + claude conversation 轮数 (extracted from `.claude/` transcript).

**Notifications** ("灵动岛"-style): native macOS notification + menubar tray window pinned `TrayBottomCenter` via `tauri-plugin-notification` + `tauri-plugin-positioner`. Triggers: child exit zero, nonzero exit, OSC agent marker, prompt-waiting heuristic, orchestrator's claude task-complete with claude-generated semantic summary. Stderr-burst is NOT a default trigger. Dedup keyed by `(plugin_id, terminal_id, event_kind)` with ~2s window.

**UI strings** are Chinese-first for MVP; English deferred to Phase 2 (i18n key layer prepared but not populated).

### Locked Decisions (every one explicitly user-confirmed, 6 rounds of questions on 2026-05-16; do not re-litigate)

**Stack / runtime**
- Tauri v2 + Rust (stable, 2024 edition) + React 18+ / TypeScript
- macOS first; cross-platform feasibility yes but not MVP requirement
- Forbidden alternatives: Electron, pure-web SPA, SwiftUI native, tmux-as-system-of-record, notebook-cell UI, MCP-server-only without Tauri shell

**Plugin contract / IPC**
- Process-isolated plugins via Tauri sidecar (OS process boundary only — no `sandbox-exec` profile, no macOS App Sandbox / per-plugin entitlements for MVP)
- Every plugin = MCP server, transport = stdio
- Self-written sidecars (community MCP servers like `GongRzhe/Gmail-MCP-Server`, `54yyyu/zotero-mcp` are reference implementations ONLY — do NOT fork/reuse)
- `plugins/<id>/plugin.toml` manifest + Rust adapter trait + frontend dir
- `build.rs` generates: Rust registry + typed IPC wrappers (ts-rs/specta) + frontend tab registry
- Adding a plugin = drop new dir under `plugins/`, rebuild; ZERO edits to host source / router / tab enum
- Raw `invoke('...')` strings are forbidden in plugin code
- Permission gating MANDATORY at the IPC dispatcher (no warning-only fallback)
- Caller identity: per-mount `PluginCapability` opaque tokens, non-serializable, not exposed on `window` / global, bound to `(plugin_id, mount_id)` at the Rust dispatcher, rotating on plugin unmount/remount
- Plugin sidecar lifecycle: each claude tab fork-execs its OWN copy of each plugin sidecar (N×M model). Host owns its own dedicated sidecar set for UI rendering
- Sidecar failure: auto-restart with exponential backoff (1s, 2s, 4s, ..., 60s cap); affected tab shows error state + manual retry button; other plugins continue

**Storage / secrets**
- Per-plugin plaintext SQLite at `${APP_DATA}/plugins/<id>/state.sqlite` with WAL mode (multi-process read)
- One file per plugin — no shared DB, no cross-plugin SQL
- Migrations owned per-plugin via `refinery` or `sqlx::migrate!`
- OAuth refresh tokens ONLY in `tauri-plugin-stronghold` keyed by `(plugin_id, account_id)`; access tokens stay in process memory; tokens NEVER appear in SQLite / WebView storage / plaintext config
- DB encryption (SQLCipher etc.) is Phase 2, NOT in MVP

**Orchestrator tab**
- Fixed top-left position in the tab strip, distinct icon vs regular plugin tabs
- Single-instance: cannot open multiple orchestrators concurrently
- Auto-launches `claude` on tab open with all platform MCP servers preconfigured
- If `claude` CLI is missing in PATH: shows onboarding card with install link
- Implemented as host's privileged tab (NOT a plugin; not under `plugins/`)
- Cross-tab read: orchestrator's `PluginCapability` for Terminal Mesh has `cross_tab_read=true`, allowing it to read any tab's scrollback
- Gmail 代发: orchestrator can request the Gmail sidecar to send on user's identity; flow is OAuth delegation (signed request, NOT token copy)

**Terminal Mesh + workspaces**
- Terminal Mesh is the plugin that owns all PTY tabs
- ≥4 concurrent PTY sessions HARD requirement (AC, user-confirmed)
- 1 MB ring buffer per PTY HARD requirement (user-confirmed)
- portable-pty + xterm.js; one Tokio actor per PTY emitting `TerminalEvent::Output | Resize | Exit | NeedsAttention`
- Bounded buffering with coalesce-on-overflow on output stream
- Sessions terminate on app exit (terminal scrollback in-memory only; persistence across restart is Phase 2)
- Each Terminal Mesh tab is bound to ONE workspace
- Workspace directory at `~/AgentPlatform/workspaces/<name>/` (user-visible) OR pointer to existing user dir (e.g. `~/repos/myproj/`); hybrid model
- NO auto git init on workspace creation
- Review path: `.claude/` transcript (agent activity) + inner cloned repos' own git (code review via "Open in Cursor")
- Workspaces persist across app restart
- One workspace = AT MOST one tab at a time; clicking an already-open workspace in the switcher focuses its existing tab
- Workspace switcher UI: `+` button → modal with "create new" (name input) OR "pick recent workspace" list
- Switcher per-row metadata: workspace name + last-used timestamp + created timestamp + claude conversation 轮数 (from `.claude/` transcript)
- All claude terminals (orchestrator + regular tabs) have full plugin MCP access — every claude can call Gmail/Papers tools. Regular tabs are workspace-isolated for filesystem (claude there sees only its own workspace dir); orchestrator has cross-tab read
- IDE handoff: "Open in Cursor" button per workspace (default Cursor, configurable VS Code/Zed/other), "Reveal in Finder" right-click

**Gmail plugin specifics**
- ≥2 independent accounts HARD requirement (AC)
- OAuth scopes: `gmail.modify` + `gmail.send` (drive send capability from orchestrator-mediated 代发邮件)
- Permanent delete (`gmail.delete` scope) is Phase 2, NOT MVP
- Send/reply UI buttons in Gmail tab are Phase 2; in MVP, send happens only via claude (any claude tab, via confirm-on-write)
- Incremental sync via `historyId`; `historyNotFound` → controlled re-sync; `429` → exponential backoff
- Multi-account state keyed by `account_id`
- Attachments: listed inline, opened on-demand via system default app; NOT downloaded, NOT indexed
- gmail crate: `google-gmail1` (Google REST, preferred over IMAP)

**Papers plugin specifics**
- Three-mode Zotero access in priority order: (1) Live HTTP API at `localhost:23119` (Zotero desktop running) → (2) SQLite read-only fallback (`zotero.sqlite ?mode=ro&immutable=1`) → (3) app-owned cached snapshot from last successful Live/SQLite read
- arXiv via `arxiv-rs`
- Dedup against Zotero by DOI / arXiv ID (NOT by title-author substring)
- Rule-based recommendations only (categories + Zotero tag affinity); personalized ML ranking is Phase 2
- Polite caching: ~30 min minimum refresh per query (DIRECTIONAL — same-order-of-magnitude OK); short rate guard on manual refresh

**Notification surface**
- Native macOS notification via `tauri-plugin-notification`
- Menubar tray window via `tauri-plugin-positioner` (`TrayBottomCenter`) as "灵动岛" analog (macOS has no real Dynamic Island; iOS Live Activities companion is Phase 2)
- Default needs-attention triggers: child exit zero, child exit nonzero, OSC agent marker, prompt-waiting heuristic (where reliably detectable). Stderr-burst is NOT a default trigger
- Orchestrator's claude task completion ALSO triggers, carrying claude-generated semantic task summary (not just "exit 0")
- Dedup keyed by `(plugin_id, terminal_id, event_kind)` with ~2s window
- Notification permission denial: graceful in-app banner fallback; tray window continues

**Write semantics**
- Confirm-on-write modal in host UI for EVERY write operation any claude tab attempts (send email, archive, mark-read, label, add Zotero item, send terminal stdin, file write outside `.claude/`)
- Per-message confirmation, NOT session-scoped trust toggles (MVP)
- Modal shows: action description + payload preview / diff + Confirm / Cancel
- Approved action then proceeds through the plugin sidecar with the appropriate capability

**UI / UX**
- Chinese-first UI strings for MVP; English deferred to Phase 2 (i18n key layer prepared)
- Shared design system across plugin tabs (host owns the design tokens)
- Top tab strip: orchestrator (leftmost, fixed) + Terminal Mesh (with sub-tabs per workspace) + Gmail + Papers + (future Phase-2 tabs)
- Workspace switcher: `+` button modal pattern (NOT sidebar, NOT command palette)

**Data retention**
- Terminal scrollback: in-memory only (lost on tab close / app exit)
- Gmail cached metadata: kept until plugin reset
- Attachments: never downloaded
- Workspace dirs: persistent until user deletes
- `.claude/` per workspace: managed by claude itself (claude's responsibility, not platform's)

### Objective Evidence (adjacent-tooling prior art; v1's evidence remains valid, augmented for v2)

- **Tauri v2 sidecar + plugin model**: https://v2.tauri.app/develop/sidecar/, https://v2.tauri.app/develop/plugins/, https://github.com/tauri-apps/plugins-workspace
- **MCP protocol**: https://modelcontextprotocol.io/specification/2025-11-25 — stdio transport, Tools/Resources/Prompts primitives, MCP Tasks abstraction (Nov 2025) for long-running work
- **MCP TS SDK** for client implementation: https://github.com/modelcontextprotocol/typescript-sdk
- **MCP Rust SDK** (for our sidecars): community implementations exist (e.g., `rmcp`); we will pick / wrap during implementation
- **Reference community MCP servers** (do NOT fork — reference only): https://github.com/GongRzhe/Gmail-MCP-Server, https://github.com/54yyyu/zotero-mcp, https://github.com/mako10k/mcp-shell-server
- **xterm.js**: https://github.com/xtermjs/xterm.js (powers VS Code terminal)
- **portable-pty**: https://docs.rs/portable-pty
- **Working precedent for Tauri v2 + xterm.js + portable-pty multi-tab terminal app**: https://github.com/crynta/terax-ai
- **google-gmail1**: https://docs.rs/google-gmail1
- **arxiv-rs**: https://docs.rs/arxiv-rs
- **Zotero local HTTP API**: https://www.zotero.org/support/dev/web_api/v3/basics; SQLite direct: https://www.zotero.org/support/dev/client_coding/direct_sqlite_database_access
- **tauri-plugin-stronghold**: secret storage with rotating master password
- **`ts-rs` / `specta`** for typed IPC code-gen from Rust SoT
- **Claude Code MCP integration**: https://code.claude.com/docs/en/mcp — `claude` supports stdio MCP servers via config; auto-launches on startup
- **MCP Dashboard prior art** (multi-server orchestrator UI): https://github.com/triepod-ai/mcp-dashboard

### Known Risks

- **N×M sidecar process count**: 3 plugins × 5 tabs = 15 lightweight processes. Each ~5–20 MB. Acceptable, but watch for activity-monitor noise. Phase-2 dedup (single sidecar per plugin behind a host proxy) is a known evolution path.
- **MCP spec evolution**: spec is community-driven (Linux Foundation since Dec 2025); SSE transport was deprecated mid-2025; Tasks abstraction is experimental. Pinning to a specific spec date (e.g. 2025-11-25) and reviewing yearly.
- **Confirm-on-write fatigue**: every claude write pops a modal. Real-world heavy use (e.g., archiving 30 emails in a row) may need a Phase-2 "trust this batch" pattern. MVP accepts the friction in exchange for safety.
- **Workspace dir bleed**: claude in workspace A doing `find /` could read files outside the workspace. Workspace isolation in MVP is *advisory* (cwd-based) not enforced by OS sandbox. Determined adversarial claude could escape. Acceptable for personal-use MVP; Phase-2 sandbox tightening planned.
- **`.claude/` schema is claude-managed**: if claude changes its on-disk schema (it evolves), our "claude conversation 轮数" metric in the workspace switcher needs to adapt. Treat as best-effort.
- **Cross-tab read privilege capture**: orchestrator's `PluginCapability` for cross_tab_read must not be leakable to other tabs. Per the AC-1.6 model, capabilities are opaque and bound; this needs to hold under React strict-mode double-mount, hot-reload, etc.
- **Gmail OAuth scope creep**: `gmail.modify + gmail.send` is broader than initial v1 scope. If the user wants narrower (e.g., read-only-via-orchestrator-only), this needs to be revisited before implementation.
- **`build.rs` scanning + codegen complexity**: emitting Rust registry + frontend tab registry + per-plugin TS wrappers from a single `build.rs` is non-trivial. Risk: stale generated files causing confusing build errors. Mitigation: clean codegen output dir on each build.
- **Single-instance orchestrator + multi-window**: if user opens multiple Tauri windows (e.g., one for Gmail, one for terminals), orchestrator's "single-instance" semantics need a clear window-level definition. Default: orchestrator lives in the primary main window; secondary windows do not have one.
- **`claude` CLI version drift**: Anthropic releases new claude versions regularly; some MCP behaviors change. We will pin a minimum supported claude version in the platform's onboarding card and surface upgrade nudges.

## Alternative Directions Considered (already evaluated, explicitly rejected — do NOT revisit during gen-plan)

### Alt-1: V1 in-process plugin model (every plugin a Rust crate compiled into the host binary, IPC via `PluginCapability` in same WebView)
- **Why not primary**: rejected on 2026-05-16 because user requires plugin process isolation + a privileged orchestrator with cross-plugin context. In-process model can't OS-isolate plugins; orchestrator-as-cross-plugin-client doesn't have a natural seam without MCP.

### Alt-2: Pure MCP-Federated Dashboard (no Tauri shell — control-room is a web app, plugins are MCP servers, claude is "the UI")
- **Why not primary**: rejected because user wants a polished desktop app with bespoke per-plugin UI (Gmail inbox view, Papers list, terminal grid), not a generic MCP server-rendered dashboard. Tauri shell with per-plugin React components is mandatory.

### Alt-3: Native SwiftUI + iOS companion for true Dynamic Island
- **Why not primary**: rejected because user prefers cross-platform reach and React/TS frontend velocity; macOS menubar tray is the agreed "灵动岛" analog. Phase-2 iOS Live Activities companion is possible but not MVP scope.

### Alt-4: tmux/Zellij-backed terminal mesh
- **Why not primary**: rejected — terminal multiplexer awkward for non-terminal tabs (Gmail, Papers); workspace isolation easier to model on portable-pty + per-tab cwd than on tmux session control.

### Alt-5: Notebook-cell workspace (Jupyter/Marimo paradigm)
- **Why not primary**: rejected — conflicts with user's explicit "tab + terminal + workspace" mental model.

### Alt-6: Auto-git-init outer workspace (review via outer git diff)
- **Why not primary**: rejected 2026-05-16 because (a) `.claude/` already records agent activity; (b) inner cloned repos' own git handles real-code review; (c) outer git on a workspace containing cloned repos creates "embedded git repository" gotchas. Workspace = plain dir.

### Alt-7: Encrypted DB / per-plugin sandbox / iOS companion app
- All Phase 2, NOT MVP.

## Synthesis Notes

The v2 architecture is the v1 architecture with three orthogonal additions:
1. **Process isolation** moves plugin runtime from in-process Rust crates to fork-exec sidecar binaries communicating via stdio MCP. The `PluginCapability` model from v1 still applies, now reinforced by OS process boundaries.
2. **Privileged orchestrator** introduces a one-off "host-owned special tab" concept. The MCP server set the orchestrator sees is the same as what any claude tab sees; what's special is (a) auto-launch, (b) the privileged capability flag for cross-tab read, (c) the delegated-OAuth-send pathway.
3. **Per-tab workspaces** make Terminal Mesh the substrate for "one problem, one terminal" workflow. Workspace = real dir + persistence + Cursor-accessible — explicitly designed to be reviewable by external IDEs.

The cost of v2 over v1:
- N×M sidecar processes (vs single host process). Mitigated by lightweight sidecars and Phase-2 dedup option.
- More codegen (Rust registry + TS wrappers + frontend tab registry). Mitigated by clean `build.rs` design.
- Confirm-on-write friction. Mitigated by Phase-2 session-trust UX once we see usage patterns.

The benefits of v2 over v1:
- Plugin crash isolation (one Gmail OAuth bug doesn't take down the app).
- Future-tab extensibility (drop a new `plugins/<id>/` dir, no host edits).
- claude-CLI-native integration (MCP stdio is exactly what `claude` consumes).
- Natural community-MCP-server pluggability (we don't fork, but the abstraction allows users to swap in third-party servers later if Phase 2 enables it).
- IDE handoff via Cursor: `~/AgentPlatform/workspaces/<name>/` is just a directory, so all developer tooling works out of the box.

This is the final architecture for v2 MVP. Open implementation decisions left for gen-plan to surface (NOT user decisions — codex can drive these):
- Exact MCP Rust SDK choice (`rmcp` vs a hand-written stdio server).
- Codegen tool choice (`ts-rs` vs `specta`).
- Migration runner choice (`refinery` vs `sqlx::migrate!`).
- React state management (Zustand vs Redux vs Context-only) — host shell concern.
- Build system orchestration (`cargo workspace` + Vite + Tauri `dev` task wiring).
- Tray icon assets / sound for notification.
- Specific log/observability shape (`tracing` + JSON structured logs probably; severity).

All "what does the user want" questions for MVP are exhausted in the Locked Decisions list above. gen-plan should treat any further "but should the user really want X" probe as a defaulting decision, not a user clarification.
```

# Required Output Format

Return EXACTLY these six sections, in this order, with these exact headings:

## CORE_RISKS
- (highest-risk IMPLEMENTATION assumptions or potential failure modes — race conditions, missing edge case handling, codegen pitfalls. NOT architectural concerns about the locked decisions.)

## MISSING_REQUIREMENTS
- (requirements likely omitted from the draft — including IMPLEMENTATION edge cases: first-run setup, Stronghold initial password flow, what happens on first launch with no plugins built, behavior when claude CLI not in PATH but user opens orchestrator tab, etc.)

## TECHNICAL_GAPS
- (specific feasibility / contract / pipeline gaps within the locked stack: how does `build.rs` emit BOTH Rust and TS files cleanly; how does the host signal a plugin sidecar to shutdown gracefully; how does confirm-on-write modal de-conflict when 2 claude tabs request writes simultaneously; how is the orchestrator's privileged capability minted without leaking globally; etc.)

## ALTERNATIVE_DIRECTIONS
- (alternatives for SUB-COMPONENT implementation choices ONLY — examples: `rmcp` crate vs hand-written stdio MCP server in Rust; `ts-rs` vs `specta` for type-gen; `refinery` vs `sqlx::migrate!` for migrations; specific React state management; etc. Each item: option + one-line tradeoff. Do NOT propose alternatives to anything in Locked Decisions.)

## QUESTIONS_FOR_USER
- (questions that genuinely need human input. If the draft says "Locked Decisions don't need user input", default to writing `- None — all user-facing decisions are locked in the draft.` UNLESS you find a real new ambiguity not covered by the draft.)

## CANDIDATE_CRITERIA
- (candidate acceptance criteria phrased as testable conditions. Target 8-14 criteria spanning plugin contract / build pipeline, MCP sidecar lifecycle, orchestrator privileges, Terminal Mesh + workspaces, Gmail multi-account, Papers three-mode Zotero, confirm-on-write, notification surface, sidecar crash recovery, error UX.)

Be specific. Vague concerns are not useful. If you have nothing material to say in a section, say so explicitly rather than padding.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_01-14-13
- Tool: codex
