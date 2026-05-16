# Ask Codex Input

## Question

ROUND-1 REASONABILITY REVIEW of Claude's candidate plan for the v2 architecture (MCP-federated process-isolated plugin platform).

# CRITICAL REMINDER

The plan's Locked Decisions are user-confirmed and NOT subject to debate this round. Do NOT propose alternatives to:
- Tauri v2 + Rust + React/TS stack
- Process-isolated plugin sidecars via stdio MCP
- N×M sidecar lifecycle (each claude tab spawns its own copies)
- Single privileged orchestrator tab (top-left, single-instance, auto-launch claude)
- Per-tab workspaces at ~/AgentPlatform/workspaces/<name>/ (user-visible, no auto git init)
- Confirm-on-write modal for every plugin-mediated write
- Stronghold-only OAuth tokens, plaintext SQLite for non-secret state
- ≥4 PTYs HARD, ≥2 Gmail accounts HARD, 1 MB ring HARD, ~30 min arXiv DIRECTIONAL
- gmail.modify + gmail.send scopes (no gmail.delete, no send/reply UI button in MVP)
- Chinese-first UI

Your job: identify REQUIRED_CHANGES and OPTIONAL_IMPROVEMENTS at the implementation/AC level, NOT architectural revision.

Output strictly these 5 sections in order with these exact headings:

## AGREE
- (specific points you accept as reasonable; cite AC/task IDs)

## DISAGREE
- (specific points you consider unreasonable; cite location; concrete reason — not style preferences)

## REQUIRED_CHANGES
- (must-fix items before convergence; each phrased as actionable edit pointing at a specific AC/task)

## OPTIONAL_IMPROVEMENTS
- (non-blocking refinements)

## UNRESOLVED
- (places where Claude and you have opposite opinions requiring user decision; format Topic: Claude=X, Codex=Y, user must choose)

---

# Candidate Plan v2-r1

# CANDIDATE PLAN v2-r1 — MCP-Federated Personal Agent Platform With Privileged Orchestrator And Per-Tab Workspaces

(Internal artifact for Codex convergence review. Will be transformed into the final plan.md after convergence.)

## Goal Description

Implement the MVP of a Tauri v2 desktop application whose three plugin tabs (Terminal Mesh, Gmail multi-account, Papers via Zotero+arXiv) run as OS-process-isolated MCP-server sidecars, plus a privileged single-instance orchestrator tab that auto-launches `claude` with full platform context. Each Terminal Mesh tab is bound to one workspace (real directory under `~/AgentPlatform/workspaces/<name>/`, user-visible and IDE-accessible). Long-running terminal/agent state surfaces through a host-owned dedup-aware notification surface combining native macOS notifications and a menubar tray window. All locked decisions in the v2 draft are constraints, not options.

## Acceptance Criteria

Following TDD: each criterion includes positive (must-pass) + negative (must-fail) tests. Quantitative thresholds marked HARD or DIRECTIONAL.

- **AC-1: Plugin Contract Pipeline is declarative and host-agnostic.** Adding a plugin requires only creating `plugins/<id>/` with `plugin.toml` + Rust adapter crate + frontend dir. The host's `build.rs` discovers plugins, emits Rust registry + frontend tab registry (`src/generated/plugin-tabs.ts`) + per-plugin TS wrappers — without edits to host source, router enums, command match arms, or tab switch statements.
  - AC-1.1: `plugin.toml` schema enforced. Required fields: `name`, `version`, `type` (`tab` for MVP), `command_bin` (sidecar binary name), `frontend` (relative path to frontend entry), `permissions[]`, `required_apis[]`, `db_namespace`, `migrations_path`.
    - Positive: a valid manifest registers the plugin and surfaces its tab at app launch with the declared permission set visible in the registry.
    - Negative: a manifest missing any required field fails the build (build.rs aborts with typed error naming the field); a manifest declaring an unknown `required_apis` value fails the build.
  - AC-1.2: Typed IPC code-gen pipeline emits BOTH Rust registry AND frontend TS wrappers. Generated outputs land at known checked-in paths (gitignored): `src-tauri/src/generated/` for Rust, `src/generated/` for TS. Each rebuild starts from clean to avoid stale artifacts.
    - Positive: TS calls go through `plugins.<id>.commands.<commandName>(args)` style generated functions; `tsc` catches wrong arg types at compile time. Removing a plugin and rebuilding produces a TS compile error for any code still calling its removed commands.
    - Negative: raw `invoke('plugin.<id>.<command>', args)` calls are not used anywhere in plugin frontend code (enforced by lint rule or convention check).
  - AC-1.3: Each plugin owns its own SQLite database file at `${APP_DATA}/plugins/<id>/state.sqlite` with WAL journaling, `busy_timeout` configured for multi-process access, and migration owned per-plugin.
    - Positive: plugin A and plugin B can run migrations concurrently without `database is locked` failures over a 100-operation stress test; deleting plugin A's `state.sqlite` resets only A.
    - Negative: the host exposes no API that allows plugin A to open or query plugin B's file.
  - AC-1.4: Permission gating is enforced at the Rust IPC dispatcher (mandatory; warning-only is forbidden). The dispatcher rejects any call whose target command requires a permission the calling plugin's manifest does not declare.
    - Positive: a plugin manifest with `permissions: ["user.email"]` can invoke Gmail-scoped commands; the dispatcher records (caller plugin_id, target command, permission match) in structured logs.
    - Negative: a plugin without `user.email` calling a Gmail-scoped command receives a typed `PermissionDenied { caller_plugin_id, missing_permission, requested_command }` error before the target ever executes.
  - AC-1.5: Caller identity is enforced via `PluginCapability` per-mount tokens. The capability is opaque (not JSON-serializable, not exposed on `window`, held only in the plugin component's React closure), bound to `(plugin_id, mount_id)` at the Rust dispatcher, and rotates on unmount/remount.
    - Positive: a permissioned wrapper invoked from plugin A's component (carrying A's capability) succeeds; the same wrapper invoked from plugin B's component fails with `PermissionDenied`.
    - Negative: calls carrying a forged token, an expired token (post-unmount), a token bound to a different `(plugin_id, mount_id)` pair, or no token at all are rejected by the dispatcher; in-flight IPC calls using a soon-to-be-invalid token are either completed atomically before rotation or rejected with a typed `CapabilityExpired` error (no half-states).
  - AC-1.6: Plugin sidecar lifecycle: host-owned UI sidecars vs per-claude-tab sidecars have distinct identities (separate client IDs, log prefixes, capability scopes). Each sidecar handles `Shutdown` signal gracefully and is force-killed after a defined timeout.
    - Positive: spawning a per-claude sidecar from a fresh `claude` tab creates a sidecar with a unique client_id visible in logs; sending `Shutdown` results in clean exit within the timeout.
    - Negative: a sidecar that ignores `Shutdown` is force-killed; orphaned sidecars do not persist in `ps aux` after host process exit.
  - AC-1.7: Sidecar crash triggers auto-restart with exponential backoff (1s → 2s → 4s → … → 60s upper cap). The affected plugin's tab shows error state during restart with a manual retry button. Other plugins are unaffected.
    - Positive: `kill -9 <gmail sidecar pid>` triggers a restart attempt at +1s, then +2s, then +4s, etc., logged with correlation IDs; Papers tab continues to render normally.
    - Negative: the Gmail tab does not silently disappear or render a white screen; orchestrator's claude sees the disconnected MCP server and surfaces an error but does not crash itself.

- **AC-2: MCP Stdio Server Discipline + Claude Wiring.**
  - AC-2.1: Every plugin sidecar exposes an MCP server over stdio with correct framing. Logs are written ONLY to stderr (or a per-process log file), NEVER to stdout (which carries MCP protocol traffic).
    - Positive: a sidecar's stdout stream contains only valid MCP framing bytes during a 60-second log-heavy test run.
    - Negative: piping the sidecar's stdout into the platform MCP parser does not throw deserialization errors when the sidecar is asked to log verbose diagnostics.
  - AC-2.2: Per-claude-tab MCP config generator. When a tab launches `claude`, the host writes an ephemeral MCP config file (e.g. `${APP_DATA}/claude-mcp-configs/<tab_id>.json`) listing exactly one sidecar invocation per plugin for that tab, then passes its path to `claude` via the documented mechanism. The file is removed when the tab closes.
    - Positive: opening a new Terminal Mesh tab and running `claude` results in `claude` having access to all MVP plugin MCP servers (visible via `claude mcp list` or equivalent inspection); tab/workspace identity is passed via env vars or config metadata so the sidecar knows which tab spawned it.
    - Negative: closing the tab removes the config file; orphan config files older than the host's lifetime are cleaned at next startup.
  - AC-2.3: `claude` CLI PATH discovery does not rely on GUI app shell inheritance. The host probes known install paths (e.g. `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`) and the user's login-shell PATH (via `bash -lc` or similar), persists the discovered path, and surfaces detection results in the orchestrator's onboarding state.
    - Positive: launching the app via Finder (no inherited terminal PATH) still finds `/opt/homebrew/bin/claude` on Apple Silicon Macs.
    - Negative: if `claude` is absent in all probed paths, the orchestrator tab shows a Chinese onboarding card listing the searched paths and a "选择 claude 路径" file picker — and does NOT spawn empty sidecar processes.

- **AC-3: Orchestrator is a privileged single-instance host tab.**
  - AC-3.1: Top-left fixed tab position with a distinct icon. The host enforces single-instance semantics: opening "Orchestrator" while one already exists focuses the existing tab.
    - Positive: clicking the orchestrator slot twice never spawns two tabs.
    - Negative: closing the orchestrator and reopening it spawns a fresh tab with rotated capabilities.
  - AC-3.2: On open, the orchestrator's PTY auto-launches `claude` with the platform's MCP config preconfigured (one sidecar per MVP plugin); if `claude` is unavailable, it shows the onboarding card (AC-2.3 negative).
    - Positive: opening the orchestrator from a cold start shows the `claude` prompt with MCP servers connected (verifiable via a quick `claude` tool listing).
    - Negative: opening with `claude` absent does NOT fork-exec any plugin sidecars.
  - AC-3.3: Cross-tab read capability for the orchestrator. The orchestrator's `PluginCapability` for the Terminal Mesh MCP server includes `cross_tab_read=true`; this flag is minted ONLY at the Rust dispatcher and is never present in any object returned to the frontend.
    - Positive: orchestrator's claude can call `terminal_mesh.read_scrollback(tab_id=X)` for any tab and receive a bounded (max-bytes) response.
    - Negative: a regular tab's claude calling the same tool for a different `tab_id` receives `PermissionDenied`. No serialization of a frontend object reveals the `cross_tab_read` flag.
  - AC-3.4: Gmail send via orchestrator's OAuth delegation. The orchestrator never receives Gmail refresh tokens; instead its claude calls `gmail.send` on the Gmail sidecar, which (after host confirm-on-write approval) reads the token from Stronghold and dispatches the send.
    - Positive: scanning host process memory + orchestrator's claude process stdin/stdout during a send operation finds no Gmail token strings.
    - Negative: an attempt to bypass the Gmail sidecar (e.g., direct Gmail API call from orchestrator's claude) does not succeed because the orchestrator's claude has no Gmail credentials of its own.
  - AC-3.5: Orchestrator's claude task-complete events trigger a notification with a semantic summary (e.g., "claude: 标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐" rather than "exit 0").
    - Positive: a long claude task completing emits one tray entry + one native notification containing claude's `task_summary` (provided via OSC sequence or MCP notification).
    - Negative: a raw `bash` exit in the orchestrator triggers the regular completion notification, not the semantic-summary path.

- **AC-4: Terminal Mesh + Workspaces.**
  - AC-4.1: At least **4 concurrent PTY sessions** can be open simultaneously [HARD requirement, user-confirmed]. Each runs a Tokio actor with a **1 MB bounded ring buffer per PTY** [HARD, user-confirmed], handling UTF-8 multi-byte and ANSI escape sequence boundaries safely (no truncation mid-codepoint or mid-escape).
    - Positive: spawn 4 PTYs running `tail -f /var/log/system.log` (or equivalent fast-output sources); all four render concurrently in xterm.js without visible blocking or garbled output. Spawning a 5th PTY succeeds.
    - Negative: closing a PTY tab while a command runs terminates the child process cleanly (verified via `ps aux` showing no orphan with the workspace's cwd); a 10MB burst into a single PTY's stdout does not OOM the host (ring overflow coalesces oldest bytes per documented policy).
  - AC-4.2: PTY supports stdin / SIGWINCH resize / exit-status reporting / cwd / env at spawn.
    - Positive: typing into a PTY reaches the child; resizing the xterm.js viewport propagates SIGWINCH; child exit code surfaces in the tab's status bar; a PTY spawned with `cwd=~/AgentPlatform/workspaces/foo` reports that cwd via `pwd`.
    - Negative: spawning with an invalid shell path returns a typed error and renders an in-tab error state without crashing the app.
  - AC-4.3: Each Terminal Mesh tab is bound to exactly ONE workspace. Workspaces live at `~/AgentPlatform/workspaces/<name>/` (user-visible, accessible by Cursor/VS Code) OR point to a user-selected existing directory (e.g. `~/repos/myproj/`).
    - Positive: creating a new workspace named "debug-arxiv" creates `~/AgentPlatform/workspaces/debug-arxiv/`; picking an existing directory associates the tab with it without copying or modifying it.
    - Negative: workspace names containing forbidden filesystem characters are rejected at the modal level with a typed error; the platform never auto-creates `.git/` in the workspace dir.
  - AC-4.4: Workspaces are persistent across app restart. One workspace = AT MOST one tab at a time, enforced by canonical-path resolution (symlinks resolved, case-insensitive paths normalized on macOS).
    - Positive: closing the app and reopening it shows the workspace switcher with previously-created workspaces; reopening a workspace from the switcher restores its tab and the previous `.claude/` conversation context.
    - Negative: clicking a workspace that is already open in another tab focuses that tab (no duplicate); creating two workspaces whose user-selected dirs resolve to the same canonical path is rejected at creation with a typed error.
  - AC-4.5: Workspace switcher UI is a modal triggered by the tab strip's `+` button. The modal has two affordances: "Create new" (name input + path source: auto OR pick existing dir) and "Pick recent" (scrollable list). Each list row shows: name + last-used timestamp + created timestamp + claude conversation rounds count (extracted from `.claude/`).
    - Positive: clicking `+` opens the modal; typing a name + clicking Create produces a new tab with the named workspace.
    - Negative: the modal is not stacked on top of a confirm-on-write modal; cancelling the modal closes it cleanly without creating any artifacts.
  - AC-4.6: Per-workspace IDE handoff. Each workspace row (in the switcher AND in an active tab's settings) exposes "Open in Cursor" (configurable to VS Code / Zed / other) and "Reveal in Finder".
    - Positive: clicking "Open in Cursor" invokes `cursor <workspace_path>` (or user-configured alternative).
    - Negative: if the configured IDE command is not in PATH, the button shows a typed error toast instead of failing silently.

- **AC-5: Gmail Plugin (multi-account, OAuth, incremental sync).**
  - AC-5.1: At least **2 Gmail accounts** added independently [HARD, user-confirmed]. OAuth via system browser loopback. Per-account state isolation in Stronghold and SQLite.
    - Positive: adding two accounts produces two switchable identities in the UI; each shows its own inbox; revoking account A externally produces a typed re-auth prompt for A without affecting B.
    - Negative: browser-launch failure during OAuth produces a typed error with a "重试" option; duplicate-account-add detection rejects re-adding the same `account_id` with a typed error.
  - AC-5.2: OAuth refresh tokens stored ONLY in `tauri-plugin-stronghold`, keyed by `(plugin_id, account_id)`. Access tokens stay in process memory.
    - Positive: scanning all plugin SQLite files post-OAuth finds zero refresh-token strings; scanning WebView storage (localStorage/sessionStorage/IndexedDB) finds zero tokens.
    - Negative: removing an account triggers Stronghold key deletion + SQLite metadata cleanup; no token survives account removal.
  - AC-5.3: Incremental sync via Gmail `historyId`. `historyNotFound` triggers controlled resync with visible progress. `429 Too Many Requests` triggers exponential backoff with a visible "syncing slower" indicator.
    - Positive: app restart resumes from last-stored `historyId`; only new messages are fetched in the test fixture.
    - Negative: an invalid `historyId` (`historyNotFound`) triggers controlled resync, not a crash; backoff is capped at a documented ceiling (avoid effective stall).
  - AC-5.4: Stronghold setup recovery. Interrupted master-password setup can be resumed or reset cleanly without leaving partially-authorized accounts.
    - Positive: killing the app during initial Stronghold setup, then relaunching, presents either "resume setup" or "reset vault" with a clear explanation of consequences.
    - Negative: no plaintext token appears in any file after an interrupted setup; orphaned partial-OAuth state in SQLite is detected and cleaned at next startup.
  - AC-5.5: Send via Gmail plugin requires confirm-on-write approval. Operation IDs ensure idempotency on sidecar retries.
    - Positive: a `gmail.send` call from any claude tab pops a confirm modal showing recipient + subject + body preview + Confirm/Cancel; confirming dispatches exactly one send; sidecar retry after approval (e.g., transient network error) does NOT cause a duplicate send because the operation ID matches the already-approved batch.
    - Negative: cancelling the confirm returns a typed `Rejected` error to claude; no Gmail API call is made.
  - AC-5.6: Attachments listed inline, opened on-demand. No auto-download, no indexing.
    - Positive: clicking an attachment opens it via macOS default app (`open` shell-out or NSWorkspace equivalent).
    - Negative: closing the inbox tab does not leave attachment temp files lingering beyond a documented cleanup window.

- **AC-6: Papers Plugin (Zotero three-mode + arXiv).**
  - AC-6.1: Zotero access in priority order: (1) **Live** — HTTP at `localhost:23119` when Zotero desktop is running and local API is enabled; (2) **SQLite read-only fallback** — `?mode=ro&immutable=1` on `zotero.sqlite` when Zotero is closed but the file is readable; (3) **App-owned cached snapshot** — the plugin's own materialized cache when both above paths fail. Each mode has a visible status indicator.
    - Positive: with Zotero running, the Live path is taken; closing Zotero falls back to SQLite-ro within one health-check cycle; deleting `zotero.sqlite` (or making it unreadable) falls back to the cached snapshot with a "offline — using snapshot from <timestamp>" banner.
    - Negative: the plugin never opens `zotero.sqlite` in read-write mode; a corrupted Zotero SQLite file is detected (deserialization error caught) and produces the cached-snapshot fallback rather than crashing.
  - AC-6.2: arXiv via `arxiv-rs`; results merged with Zotero items and deduplicated by DOI / arXiv ID.
    - Positive: an arXiv item already present in Zotero by DOI appears once with an "in-library" badge.
    - Negative: items with same author/title prefix but distinct DOIs are NOT collapsed.
  - AC-6.3: Rule-based recommendations only (categories + Zotero tag affinity). No personalized ranking.
    - Positive: subscribing to `cs.LG` produces a daily-fresh list filtered by current arXiv submissions.
    - Negative: removing a subscription stops new items appearing; no implicit "remember-my-clicks" behavior.
  - AC-6.4: Polite caching: target **~30 minute minimum refresh interval per configured query** [DIRECTIONAL, user-confirmed — same-order-of-magnitude OK]. Manual refresh respects a short rate guard.
    - Positive: clicking refresh twice rapidly produces only one network call.
    - Negative: programmatic poll rate does not exceed configured minimum interval by more than one order of magnitude.

- **AC-7: Confirm-on-Write Surface.**
  - AC-7.1: Every plugin-mediated write originating from a claude tab triggers a host-owned confirm modal showing action description + payload preview + Confirm / Cancel.
    - Positive: a `gmail.label` call shows a modal with "添加标签 'Important' 到邮件 X"; confirming labels the email, cancelling returns `Rejected` to claude.
    - Negative: a read-only call (e.g., `gmail.list_inbox`) does NOT trigger a confirm modal.
  - AC-7.2: Concurrent write requests across tabs are arbitrated by a host-owned global queue, not stacked modals.
    - Positive: two simultaneous `gmail.label` calls from two tabs are serialized; the second modal appears after the first is dismissed.
    - Negative: there is never more than one open confirm modal at a time; queued requests show a "pending approval" indicator in their originating tab.
  - AC-7.3: Approval carries an idempotent operation ID. A sidecar retrying after approval (e.g., transient error) must reuse the same operation ID to avoid duplicate writes.
    - Positive: a sidecar retry with the same operation ID against an already-completed approval returns the cached result, not a fresh write attempt.
    - Negative: a retry with a different operation ID is treated as a new request requiring a fresh approval.
  - AC-7.4: Terminal stdin confirmation (claude writing into another tab's PTY via orchestrator) pauses the write but does NOT block the destination PTY's output stream, resize events, or exit observation.
    - Positive: while a confirm-on-write modal is open for stdin, output from the destination PTY continues to render; the PTY can be resized; if the child exits, the exit is observed and surfaced.
    - Negative: cancelling the stdin confirm does not corrupt the destination PTY's state.
  - AC-7.5: Scope honesty: confirm-on-write covers ONLY plugin-mediated writes (Gmail send/label/etc., Papers add/edit, Terminal stdin via orchestrator). Raw filesystem writes by claude inside a workspace's own dir are NOT intercepted in MVP. This boundary is documented in the orchestrator onboarding card and the project README.
    - Positive: a `gmail.send` call IS gated by confirm-on-write.
    - Negative: claude writing `notes.md` in its own workspace via `Edit` or `Write` tool is NOT gated (and is not claimed to be).

- **AC-8: Notification Surface ("灵动岛" analog).**
  - AC-8.1: Native macOS notification (`tauri-plugin-notification`) AND menubar tray window (`tauri-plugin-positioner` `TrayBottomCenter`) both fire on terminal completion.
    - Positive: `sleep 10 && echo done` in a PTY produces one banner + one tray entry.
    - Negative: a tight loop of fast-completing commands produces at most one notification per `(plugin_id, terminal_id, event_kind)` per ~2 second dedup window.
  - AC-8.2: Default needs-attention triggers: child exit 0, nonzero exit, OSC agent marker, prompt-waiting heuristic (where reliably detectable). Stderr-burst is NOT a default trigger.
    - Positive: emitting an OSC agent marker sequence triggers a needs-attention tray entry.
    - Negative: writing a large blob to stderr does NOT trigger an alert.
  - AC-8.3: Orchestrator's claude task-complete fires a notification with a claude-generated semantic summary (passed via OSC or MCP notification protocol).
    - Positive: a multi-step claude task completing surfaces a summary like "标记 3 封邮件为已读" rather than "exit 0".
    - Negative: a non-claude command in the orchestrator triggers the regular completion event, not the semantic-summary path.
  - AC-8.4: Notification permission denial is graceful.
    - Positive: launching with notifications denied shows an in-app banner explaining the fallback; tray window continues to drive event surfacing.
    - Negative: denied permission does NOT cause a runtime panic or repeated re-prompts.

- **AC-9: Application Shell Robustness + First-Run Bootstrap + Quit Sequencing.**
  - AC-9.1: First-run bootstrap creates required dirs: `~/AgentPlatform/`, `~/AgentPlatform/workspaces/`, `${APP_DATA}/plugins/<id>/`, log dir, `${APP_DATA}/claude-mcp-configs/`. Permission failures surface a user-readable error.
    - Positive: first launch on a fresh user account creates all required dirs and shows the empty workspace switcher.
    - Negative: a permission failure (e.g., read-only home dir) shows a Chinese error card with the offending path and remediation hint instead of crashing.
  - AC-9.2: Cold start with no plugin sidecars built/bundled shows a dev-diagnostics page listing missing binaries and a hint to run the build command.
    - Positive: in a dev checkout where `cargo build` has not been run, the app launches and the orchestrator tab shows "缺失 plugin 二进制：…", listing each expected binary.
    - Negative: production builds with all sidecars bundled never show this diagnostics page.
  - AC-9.3: SQLite migration failure for one plugin blocks that plugin's mount, shows error state in its tab, and leaves other plugins unaffected.
    - Positive: simulating a migration error in Gmail's startup keeps Papers + Terminal Mesh running; Gmail tab shows an error state with a "重试迁移" button.
    - Negative: migration failure does NOT crash the host process.
  - AC-9.4: App quit sequence: stop accepting new writes → resolve/cancel pending confirm modals → terminate PTYs (SIGTERM + grace + SIGKILL) → send graceful Shutdown to all sidecars → force-kill after timeout. No orphaned sidecar or PTY child processes remain.
    - Positive: `lsof` and `ps aux` after a normal quit show no orphan processes attributable to the app.
    - Negative: force-quitting via OS still triggers SIGTERM cleanup hooks where possible; persistent orphans are documented as a known edge case.
  - AC-9.5: Structured JSON logs per process with correlation IDs (`tab_id`, `plugin_id`, `request_id`). OAuth tokens and email bodies are redacted in logs.
    - Positive: any request/event log entry carries correlation IDs that can be joined across host + sidecar logs.
    - Negative: grepping logs for token-shaped strings (long base64-ish strings near `refresh_token` keys) returns zero matches.

## Path Boundaries

> Lower Bound = minimum implementation that still passes ALL ACs. Upper Bound = polished MVP. Slack lives in UI polish, error-message richness, observability depth — NOT in AC-required behaviors.

### Upper Bound (Maximum Acceptable Scope)
- **Plugin contract**: full manifest schema (with version, migrations_path, optional metadata for icons / sort order), polished error UI per plugin, generated source dir with `// AUTOGENERATED — do not edit` headers, lint rule banning raw `invoke('plugin.…')` strings, `cargo make`-style build orchestration.
- **MCP wiring**: per-tab MCP config files with automatic cleanup on tab close AND at app startup garbage collection; sidecar log files rotated by size; structured JSON dev logs with correlation IDs.
- **Orchestrator**: polished Chinese onboarding card with `claude` path picker; semantic-summary notification with action-count breakdown; cross-tab read with bounded byte cap visible to user in settings.
- **Terminal Mesh**: ≥4 PTYs (HARD), 1 MB ring (HARD), UTF-8/ANSI-safe boundary handling, polished tab-close animations, drag-to-reorder, workspace switcher modal with debounced search.
- **Gmail**: multi-account read + modify (label/archive/mark-read/mark-unread/trash) + send. Polished compose UI in confirm-on-write modal showing diff against current state. Attachment list/open-on-demand. Polished re-auth UX.
- **Papers**: full three-mode Zotero with visible mode status indicator; arXiv polite caching with per-query refresh times shown; in-library badge with hover tooltip showing Zotero item link.
- **Confirm-on-write**: global queue with pending-approval count badge in tab strip; idempotent operation IDs surfaced in approval modal for debug; per-batch approval not supported in MVP (single-action only).
- **Notifications**: native + tray with semantic summaries from claude; permission-denial fallback banner; dedup arbiter exposes its window/keys in dev tools.
- **Application shell**: structured JSON logs with redaction rules; quit sequencing with progress indicator if termination is slow; Chinese UI strings throughout with `i18n` key layer ready for Phase-2 English.
- **Packaging**: macOS signed/notarized `.app` is STRETCH, not required.

### Lower Bound (Minimum Acceptable Scope — still passes ALL ACs)
- Plugin contract: manifest enforced, codegen pipeline working, per-plugin DB files, mandatory permission gating, PluginCapability per-mount tokens, distinct host-UI vs per-claude sidecar identities, auto-restart with exponential backoff (all AC-1 sub-criteria required).
- MCP wiring: stdio framing with logs to stderr; per-tab MCP config generator works (cleanup of orphan configs may be at next startup rather than realtime); `claude` PATH discovery probes known paths + login shell PATH (AC-2 required).
- Orchestrator: top-left fixed single-instance tab auto-launches claude; cross-tab read works for orchestrator only; Gmail send via OAuth delegation works; semantic-summary notification works (AC-3 required).
- Terminal Mesh: ≥4 concurrent PTYs HARD; 1 MB ring HARD; UTF-8/ANSI-safe slicing; workspace creation + persistence + single-tab uniqueness; switcher modal works; Open in Cursor + Reveal in Finder.
- Gmail: ≥2 accounts HARD; OAuth via system browser; Stronghold-only tokens; historyId incremental sync; rate-limit backoff; interrupted-Stronghold recovery; send via confirm-on-write with operation IDs (AC-5 required); attachment list/open-on-demand.
- Papers: all three Zotero modes; DOI/arXiv-ID dedup; rule-based recommendations; polite caching (~30 min DIRECTIONAL).
- Confirm-on-write: every plugin-mediated write gated; global queue prevents stacked modals; operation IDs for idempotency; terminal stdin pause without output block; scope honesty documented.
- Notifications: native + tray on completion; dedup keyed by `(plugin_id, terminal_id, event_kind)` ~2s; semantic-summary path for orchestrator's claude; permission-denial graceful fallback.
- Application shell: first-run bootstrap; dev-diagnostics page when no plugins built; plugin migration failure plugin-scoped; quit sequencing with no orphans; structured logs with correlation IDs and redaction.
- Chinese UI strings (DEC-11 locked).

### Allowed Choices
- **Can use**: Tauri v2; React 18+ / TypeScript; Rust (stable, 2024 edition); `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold`, `rusqlite` (with WAL + busy_timeout); `google-gmail1` (Gmail REST); `arxiv-rs`; Zotero local HTTP API + `zotero.sqlite` read-only.
- **For implementation choice** (deferred to coding phase, NOT user decisions): MCP Rust SDK (`rmcp` vs hand-written stdio), type-codegen (`ts-rs` vs `specta`), migration runner (`refinery` vs `sqlx::migrate!`), React state mgmt (Zustand vs Context+reducer), structured-log shape (JSON via `tracing` vs custom), confirm-on-write queue scope (global vs per-plugin — global picked at Lower Bound).
- **Cannot use**: Electron; pure-web SPA without desktop shell; SwiftUI / AppKit-native; tmux-as-system-of-record; notebook-cell UI; in-process plugins (only sidecar processes); raw `invoke('plugin.…')` strings in plugin code; hand-maintained plugin enumeration in router; encrypted SQLite for MVP (Stronghold for secrets only); community-MCP-server forks (self-written sidecars only); macOS App Sandbox / per-plugin entitlements for MVP (OS process boundary only); send/reply UI buttons in Gmail tab (Phase 2 — send is via claude only in MVP); per-plugin sidecar dedup proxy (N×M model accepted for MVP).

## Feasibility Hints and Suggestions

### Conceptual Approach

1. **Host skeleton** — Tauri v2 scaffold + React/TS shell. Tab strip with fixed orchestrator slot at top-left and a `+` for workspace tabs. Backend exposes `PluginHost` trait + IPC dispatcher.
2. **Plugin contract** — `plugins/<id>/plugin.toml` + Rust adapter crate + frontend dir. `build.rs` discovers plugins, emits Rust registry (`src-tauri/src/generated/plugin_registry.rs`) + frontend tab registry (`src/generated/plugin-tabs.ts`) + per-plugin TS wrappers (`src/generated/plugins/<id>.ts`). Generated dirs are `.gitignore`d but regenerated from clean each build to avoid stale artifacts.
3. **Typed IPC code-gen** — pick one of `ts-rs` or `specta` (implementation detail, decide during coding). Rust command/event/error types are the source of truth. Generated TS wrappers expose `plugins.<id>.commands.<name>(args)` and `plugins.<id>.events.<name>(payload => {...})`.
4. **PluginCapability** — `Arc<PluginCapability>` opaque struct holding `(plugin_id, mount_id, signed_token)`. Issued at mount via React `usePluginCapability` hook that receives a fresh capability from the host. Generated wrappers accept the capability via React Context. Tokens rotate on unmount/remount via host's `MountRegistry`.
5. **MCP stdio sidecars** — pick one MCP Rust SDK (`rmcp` or hand-written wrapper around `tokio::io::stdin`/`stdout` for stdio framing). Logs go to `${APP_DATA}/logs/<plugin_id>/<client_id>.log`, NEVER to stdout. Sidecar shutdown contract: receive `Shutdown` IPC → flush state → exit; host waits up to a timeout then SIGKILL.
6. **claude wiring** — host generates `${APP_DATA}/claude-mcp-configs/<tab_id>.json` listing one sidecar invocation per plugin (e.g., `{"mcpServers": {"gmail": {"command": "/path/to/gmail-plugin", "args": ["--client-id", "claude:<tab_id>", "--workspace", "<workspace_path>"]}, …}}`). Tab launches `claude --mcp-config <path>` (exact flag determined by `claude` CLI version). Config cleanup on tab close + startup GC.
7. **claude PATH discovery** — on first launch, probe `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`; if missing, run `bash -lc 'command -v claude'` to get login-shell PATH; persist the result. If still missing, orchestrator shows onboarding card.
8. **Per-plugin SQLite** — each plugin opens `${APP_DATA}/plugins/<id>/state.sqlite` with `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;` and uses one of `refinery` or `sqlx::migrate!` (implementation choice) for migrations. Migration runs under an advisory lock to prevent concurrent runs across multiple sidecar copies.
9. **Stronghold** — refresh tokens at `(plugin_id, account_id)` keys. Master password setup on first OAuth attempt with resume/reset on interrupt detection (presence of a partial-state marker file in `${APP_DATA}/stronghold-state/`).
10. **Terminal Mesh** — `portable-pty` Tokio actor with bounded 1 MB ring (`VecDeque<u8>` capped + overflow drop-oldest-with-coalesce). UTF-8/ANSI-safe: don't slice mid-codepoint or mid-escape; use a buffer scanner to find safe boundaries. xterm.js subscribes via typed events.
11. **Workspace persistence** — central registry at `${APP_DATA}/workspaces.json` listing canonical paths, names, timestamps. Each workspace dir optionally has `.workspace.json` with display metadata (overrides registry). Conversation rounds counted by parsing `.claude/` files (best-effort).
12. **Confirm-on-write queue** — host-owned arbiter receives requests from sidecars (via host IPC), queues them, opens one modal at a time, replies with `Approved { operation_id }` or `Rejected`. Sidecar's pending writes hold an `oneshot` channel waiting for the reply.
13. **Notification surface** — host subscribes to plugin events with `notify` capability declared. Dedup keyed by `(plugin_id, terminal_id, event_kind)` over ~2s window. Tray window uses `tauri-plugin-positioner` `TrayBottomCenter`; native via `tauri-plugin-notification`.
14. **App quit** — sequence enforced by an `AppQuitCoordinator`: stop write acceptance → resolve modals → SIGTERM PTYs → graceful sidecar Shutdown → SIGKILL after timeout.

### Specification Deliverables (produced by `analyze`-tagged tasks)
- `docs/specs/plugin-contract.md` — manifest schema (every field + validation), codegen contract, dispatcher rules, PluginCapability lifecycle, permission enumeration.
- `docs/specs/mcp-sidecar.md` — stdio framing, log channel rules, shutdown contract, sidecar identity/client_id scheme, host-UI vs per-claude distinction, per-tab config file format.
- `docs/specs/claude-launch.md` — PATH discovery, MCP config generation, version requirement, onboarding card flow.
- `docs/specs/terminal-events.md` — needs-attention taxonomy, ring buffer overflow policy, UTF-8/ANSI safety, dedup keys.
- `docs/specs/confirm-on-write.md` — queue semantics, idempotent operation IDs, terminal stdin pause-without-output-block, scope honesty about non-plugin-mediated writes.
- `docs/specs/gmail-sync.md` — OAuth loopback + browser-launch-failure handling, historyId/historyNotFound recovery, 429 backoff curve, initial-full-sync boundaries, account_id keying, Stronghold interrupted-setup recovery.

### Relevant References
- Tauri v2 architecture: https://v2.tauri.app/concept/architecture/
- Tauri v2 plugin & sidecar: https://v2.tauri.app/develop/plugins/, https://v2.tauri.app/develop/sidecar/
- MCP spec (2025-11-25): https://modelcontextprotocol.io/specification/2025-11-25
- Claude Code MCP: https://code.claude.com/docs/en/mcp
- xterm.js, portable-pty, google-gmail1, arxiv-rs (as cited in draft)
- Working precedent — Terax AI: https://github.com/crynta/terax-ai

## Dependencies and Sequence

### Milestones

1. **Plugin Contract Foundation** — substrate; before any plugin code.
   - Phase A: Tauri v2 scaffold + React/TS shell + fixed-orchestrator-slot tab strip.
   - Phase B: Write `docs/specs/plugin-contract.md` (manifest, dispatcher, PluginCapability lifecycle, permissions, codegen contract).
   - Phase C: `build.rs` codegen pipeline (Rust registry + frontend tab registry + per-plugin TS wrappers; clean-on-build).
   - Phase D: Generated Rust IPC dispatcher with permission gating + PluginCapability validation + structured logging.
   - Phase E: Per-plugin SQLite framework (WAL + busy_timeout + per-plugin migrations).
   - Phase F: tauri-plugin-stronghold integration with master-password setup/resume/reset.
   - Phase G: Frontend lifecycle hooks (`usePluginCapability`, `onMount`/`onUnmount`/`onError`) + host error boundary.
   - Phase H: First-run bootstrap (`~/AgentPlatform/`, app-data, log dirs) + dev-diagnostics page when no plugins.

2. **MCP Sidecar Discipline + Claude Wiring** — universal across plugins.
   - Phase A: Write `docs/specs/mcp-sidecar.md`.
   - Phase B: MCP stdio framing helper (logs→stderr/files; sidecar shutdown contract; client_id scheme).
   - Phase C: Sidecar lifecycle manager (spawn/restart with exp backoff; graceful Shutdown; distinct host-UI vs per-claude identities).
   - Phase D: Write `docs/specs/claude-launch.md`.
   - Phase E: claude PATH discovery + per-tab MCP config generator + onboarding card.

3. **Terminal Mesh + Workspaces** — first plugin; stress-tests the contract.
   - Phase A: Write `docs/specs/terminal-events.md` (taxonomy + ring buffer overflow + UTF-8/ANSI boundary).
   - Phase B: `portable-pty` Tokio actor with 1 MB ring + cancellation + UTF-8/ANSI-safe slicing.
   - Phase C: xterm.js multi-tab frontend with concurrent-PTY stress test (≥4 PTYs).
   - Phase D: Workspace storage + canonical-path uniqueness + persistence registry.
   - Phase E: Workspace switcher modal (+ button); per-row metadata.
   - Phase F: Open in Cursor / Reveal in Finder.

4. **Orchestrator** — privileged single-instance tab.
   - Phase A: Top-left fixed single-instance tab (host-owned, NOT a plugin).
   - Phase B: Cross-tab read privileged capability path (bounded API at dispatcher).
   - Phase C: Gmail send via OAuth delegation pathway.
   - Phase D: Notification surface integration (tauri-plugin-notification + positioner + dedup arbiter) — depends on event taxonomy from M3-A.
   - Phase E: Orchestrator semantic-summary notification + claude task-complete handling.

5. **Confirm-on-Write + Write Queue** — host-level write gating.
   - Phase A: Write `docs/specs/confirm-on-write.md` (queue semantics, operation IDs, terminal-stdin pause-without-output-block, scope honesty).
   - Phase B: Global confirm-on-write queue arbiter + modal UI.
   - Phase C: Terminal stdin confirmation wiring (Terminal Mesh side).

6. **Gmail Plugin** — depends on Stronghold + dispatcher + sidecar lifecycle + confirm queue.
   - Phase A: Write `docs/specs/gmail-sync.md`.
   - Phase B: Gmail sidecar scaffold (MCP server, OAuth loopback flow with browser launch failure handling).
   - Phase C: Multi-account Stronghold storage + duplicate detection + interrupted-setup recovery.
   - Phase D: Incremental sync engine (historyId, historyNotFound, 429 backoff).
   - Phase E: Send via confirm-on-write with idempotent operation IDs.
   - Phase F: Inbox UI (Chinese strings) + account switcher + attachment list/open-on-demand.

7. **Papers Plugin** — depends on SQLite framework.
   - Phase A: Papers sidecar scaffold (MCP server).
   - Phase B: Zotero Live HTTP probe + SQLite read-only fallback + app-owned cached snapshot.
   - Phase C: arxiv-rs query engine + rule-based recommendations + polite caching guard.
   - Phase D: Papers tab UI (Chinese strings) with merged list + DOI/arXiv-ID dedup + offline indicator.

8. **Robustness, Logging, Quit** — cross-cutting.
   - Phase A: Structured JSON logging with correlation IDs and redaction rules.
   - Phase B: SQLite migration-failure plugin-scoped recovery UI.
   - Phase C: AppQuitCoordinator (write-acceptance freeze → modal cancellation → PTY teardown → graceful sidecar shutdown → force-kill timeout).

Relative dependencies:
- M1 must be largely complete before M2 starts.
- M2 must be functional before M3-C (Terminal Mesh frontend wiring) needs claude.
- M3 + M4 can proceed mostly in parallel after M2.
- M5 must be functional before M6-E (Gmail send) and M3-C orchestrator stdin (M4-C feeds M5).
- M6 and M7 can proceed in parallel after M5.
- M8 runs alongside throughout.

## Task Breakdown

| Task ID | Description | Target AC | Tag | Depends On |
|---------|-------------|-----------|-----|------------|
| task1 | Scaffold Tauri v2 + React/TS shell with fixed-orchestrator-slot tab strip + first-run bootstrap | AC-9.1 | coding | - |
| task2 | Write `docs/specs/plugin-contract.md` (manifest schema, dispatcher rules, PluginCapability lifecycle, permission enum, codegen contract) | AC-1.1, AC-1.4, AC-1.5 | analyze | task1 |
| task3 | `build.rs` codegen pipeline emitting Rust registry + frontend tab registry + per-plugin TS wrappers; clean-rebuild semantics + checked-in gitignored generated dirs | AC-1.1, AC-1.2 | coding | task2 |
| task4 | Generated Rust IPC dispatcher (permission gating + PluginCapability validation by `(plugin_id, mount_id)` + structured logs with correlation IDs) | AC-1.4, AC-1.5, AC-9.5 | coding | task3 |
| task5 | Per-plugin SQLite framework (WAL, busy_timeout, per-plugin migration runner, plugin-scoped migration-failure recovery UI) | AC-1.3, AC-9.3 | coding | task1 |
| task6 | tauri-plugin-stronghold integration with master-password setup/resume/reset and interrupted-state detection | AC-4.3, AC-5.4 (cross-ref) | coding | task1 |
| task7 | Frontend lifecycle hooks (`usePluginCapability`, `onMount`/`onUnmount`/`onError`) + host error boundary + capability-rotation race safety | AC-1.5, AC-6.1 | coding | task4 |
| task8 | Dev diagnostics page (cold start with no plugin sidecars built) | AC-9.2 | coding | task1 |
| task9 | Write `docs/specs/mcp-sidecar.md` (stdio framing, log channel rules, shutdown contract, client_id scheme, host-UI vs per-claude identities) | AC-2.1, AC-1.6 | analyze | task1 |
| task10 | MCP stdio framing helper crate (logs→stderr/files; structured shutdown signaling) | AC-2.1, AC-1.6 | coding | task9 |
| task11 | Sidecar lifecycle manager (spawn, exp-backoff auto-restart 1s→60s, graceful Shutdown, distinct client identities) | AC-1.7, AC-1.6 | coding | task10 |
| task12 | Write `docs/specs/claude-launch.md` (GUI app PATH discovery, MCP config file format, claude version handling, onboarding card flow) | AC-2.3, AC-3.2 | analyze | task1 |
| task13 | claude PATH discovery + per-tab MCP config generator at `${APP_DATA}/claude-mcp-configs/<tab_id>.json` with cleanup + onboarding card if missing | AC-2.2, AC-2.3, AC-3.2 | coding | task12, task11 |
| task14 | Write `docs/specs/terminal-events.md` (event taxonomy + dedup keys + ring buffer overflow policy + UTF-8/ANSI boundary safety) | AC-8.2 | analyze | task1 |
| task15 | portable-pty Tokio actor with 1 MB ring buffer (UTF-8/ANSI-safe slicing, coalesce-on-overflow), cancellation, TerminalEvent stream | AC-4.1, AC-4.2 | coding | task14, task10 |
| task16 | xterm.js multi-tab frontend wiring; concurrent-PTY stress test verifying ≥4 PTYs simultaneously | AC-4.1 | coding | task15 |
| task17 | Workspace storage layer: `${APP_DATA}/workspaces.json` registry + canonical-path resolution (symlinks, case-insensitive macOS) + open-tab uniqueness + first-launch creation of `~/AgentPlatform/workspaces/` | AC-4.3, AC-4.4, AC-9.1 | coding | task5 |
| task18 | Workspace switcher modal (`+` button) with create / pick-existing + per-row metadata (name, last used, created, conversation rounds count from `.claude/`) | AC-4.5 | coding | task17, task7 |
| task19 | Open in Cursor + Reveal in Finder per workspace (configurable IDE command); typed error if IDE binary not in PATH | AC-4.6 | coding | task18 |
| task20 | Orchestrator host-side tab: top-left fixed, single-instance lock, auto-launch claude on open with MCP config preconfigured | AC-3.1, AC-3.2 | coding | task13, task16 |
| task21 | Cross-tab read privileged capability path (orchestrator's PluginCapability for Terminal Mesh sets `cross_tab_read=true`; bounded API max-bytes; minted at dispatcher only) | AC-3.3 | coding | task20, task15 |
| task22 | Notification surface: tauri-plugin-notification + tauri-plugin-positioner TrayBottomCenter + dedup arbiter keyed by `(plugin_id, terminal_id, event_kind)` + permission-denial fallback | AC-8.1, AC-8.2, AC-8.4 | coding | task14, task20 |
| task23 | Orchestrator semantic-summary notification path (OSC / MCP notification → tray entry with claude task summary) | AC-3.5, AC-8.3 | coding | task22, task20 |
| task24 | Write `docs/specs/confirm-on-write.md` (queue semantics, idempotent operation IDs, terminal-stdin pause-without-output-block, scope honesty about non-plugin-mediated writes) | AC-7.1, AC-7.5 | analyze | task1 |
| task25 | Global confirm-on-write queue arbiter (single-modal-at-a-time, operation IDs, queue UI in tabs) | AC-7.1, AC-7.2, AC-7.3 | coding | task24, task4 |
| task26 | Terminal stdin confirm-on-write wiring (Terminal Mesh side: pause write until approval without blocking output/resize/exit observation) | AC-7.4 | coding | task25, task15 |
| task27 | Write `docs/specs/gmail-sync.md` (OAuth loopback flow + browser-launch failure handling, historyId/historyNotFound recovery, 429 backoff curve, account_id keying, initial-full-sync boundaries, Stronghold interrupted-setup recovery) | AC-5.1, AC-5.2, AC-5.3, AC-5.4 | analyze | task6 |
| task28 | Gmail sidecar scaffold: MCP server skeleton + OAuth loopback flow + browser-launch failure UX + duplicate-account detection | AC-5.1, AC-2.1 | coding | task27, task11 |
| task29 | Multi-account Stronghold storage (`plugin_id × account_id` keying) + Stronghold interrupted-setup detection and recovery flow | AC-5.2, AC-5.4 | coding | task28, task6 |
| task30 | Gmail incremental sync engine (historyId, historyNotFound controlled resync, 429 exponential backoff with visible "syncing slower") | AC-5.3 | coding | task29, task5 |
| task31 | Gmail send via confirm-on-write with idempotent operation IDs (sidecar retries do not duplicate) | AC-5.5, AC-7.3 | coding | task30, task25 |
| task32 | Gmail inbox UI (Chinese strings) + account switcher + attachment list/open-on-demand | AC-5.1, AC-5.6 | coding | task31 |
| task33 | Papers sidecar scaffold: MCP server skeleton | AC-2.1 | coding | task11 |
| task34 | Zotero three-mode access client (Live HTTP probe at `localhost:23119` → SQLite-ro fallback with `?mode=ro&immutable=1` → app-owned cached snapshot; visible status indicator) | AC-6.1 | coding | task33, task5 |
| task35 | arxiv-rs query engine + DOI/arXiv-ID dedup + rule-based recommendations + polite caching guard (~30 min DIRECTIONAL) | AC-6.2, AC-6.3, AC-6.4 | coding | task34 |
| task36 | Papers tab UI (Chinese strings) with merged list + dedup badges + offline indicator | AC-6.1, AC-6.2 | coding | task35 |
| task37 | Structured JSON logging with correlation IDs (`tab_id`, `plugin_id`, `request_id`) + redaction rules for OAuth + email bodies | AC-9.5 | coding | task4 |
| task38 | AppQuitCoordinator: write-acceptance freeze → resolve/cancel pending modals → SIGTERM PTYs (+grace+SIGKILL) → graceful sidecar Shutdown → force-kill after timeout (no orphans) | AC-9.4 | coding | task11, task15, task25 |

## Claude-Codex Deliberation (post round 1)

### Agreements (so far)
- Locked Decisions in v2 draft are absolute constraints; codex Phase 3 explicitly returned QUESTIONS_FOR_USER = "None — all user-facing decisions are locked".
- build.rs codegen pipeline needs clean-on-build semantics + gitignored generated dirs; Cargo OUT_DIR alone is not Vite-friendly.
- MCP stdio sidecars MUST log to stderr/files only — stdout is protocol traffic.
- Per-claude-tab MCP config file approach for wiring `claude` to plugin sidecars.
- `claude` PATH discovery must NOT rely on GUI app shell inheritance.
- Confirm-on-write needs a global queue arbiter, not stacked modals; idempotent operation IDs prevent duplicate writes on sidecar retry.
- Terminal stdin confirmation pauses the write but not the output/resize/exit observation channels.
- Scope honesty: confirm-on-write covers plugin-mediated writes only; raw filesystem writes inside a workspace are NOT intercepted in MVP.
- Stronghold interrupted-setup recovery is a real edge case requiring deterministic resume/reset flow.
- SQLite multi-process WAL access requires `busy_timeout` + migration-locking discipline.
- App quit needs a coordinator with sequenced steps.
- Structured JSON logs with correlation IDs and redaction.

### Implementation-Decision Items (NOT user decisions — deferred to coding phase)
- MCP Rust SDK: `rmcp` vs hand-written stdio wrapper.
- Type-codegen tool: `ts-rs` vs `specta`.
- Migration runner: `refinery` vs `sqlx::migrate!`.
- React state management: Zustand vs Context+reducer.
- Confirm-on-write queue: global (chosen for Lower Bound) vs per-plugin queues.
- Generated source dir: committed vs build-only (chosen: gitignored, build-only).
- SQLite access pattern: pool per sidecar vs actor-owned connection.

### Convergence Status
- Round: 1 (initial candidate). Pending round-2 review.

## Pending User Decisions

- None. Codex Phase 3 explicitly returned `QUESTIONS_FOR_USER: None — all user-facing decisions are locked in the draft.`. All architectural decisions are pre-locked via 6 rounds of user clarification captured in the v2 draft's "Locked Decisions" section.

## Implementation Notes

### Code Style Requirements
- Implementation code and comments must NOT contain plan-specific terminology such as "AC-", "Milestone", "Step", "Phase", or similar workflow markers.
- These terms are for plan documentation only, not for the resulting codebase.
- Use descriptive, domain-appropriate naming in code instead (e.g., the host trait is `PluginHost`, not `MilestoneOnePluginHost`).

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_01-21-19
- Tool: codex
