# Ask Codex Input

## Question

ROUND-2 REASONABILITY REVIEW. Claude applied all 8 round-1 REQUIRED_CHANGES (branded-handle PluginCapability, generated-dir wording fix, same-plugin SQLite contention, traceability fixes for task6/task7, Gmail modify-ops task added, AC-3.4 deterministic test, force-quit honesty, no compose/send UI buttons) and incorporated round-1 optional improvements (manifest uniqueness, per-command permission metadata, portable test fast-output, 429 ceiling 5min, attachment 60s cleanup, Zotero SQLite path override, lazy notification permission, logging folded into task4).

Confirm correctness of round-1 application. Identify any NEW required changes that emerged from v1→r2 edits. If none, say convergence is reached.

Output the 5 sections: AGREE / DISAGREE / REQUIRED_CHANGES / OPTIONAL_IMPROVEMENTS / UNRESOLVED. Be terse.

Locked Decisions are still not subject to debate.

---

# Candidate Plan v2-r2

# CANDIDATE PLAN v2-r2 — MCP-Federated Personal Agent Platform With Privileged Orchestrator And Per-Tab Workspaces

(Round-2 candidate after applying Codex round-1 REQUIRED_CHANGES + selected OPTIONAL_IMPROVEMENTS.)

## Goal Description

Implement the MVP of a Tauri v2 desktop application whose three plugin tabs (Terminal Mesh, Gmail multi-account, Papers via Zotero+arXiv) run as OS-process-isolated MCP-server sidecars, plus a privileged single-instance orchestrator tab that auto-launches `claude` with full platform context. Each Terminal Mesh tab is bound to one workspace (real directory under `~/AgentPlatform/workspaces/<name>/`, user-visible and IDE-accessible). Long-running terminal/agent state surfaces through a host-owned dedup-aware notification surface combining native macOS notifications and a menubar tray window. All locked decisions in the v2 draft are constraints, not options.

## Acceptance Criteria

Following TDD: each criterion includes positive (must-pass) + negative (must-fail) tests. Quantitative thresholds marked HARD or DIRECTIONAL.

- **AC-1: Plugin Contract Pipeline is declarative and host-agnostic.** Adding a plugin requires only creating `plugins/<id>/` with `plugin.toml` + Rust adapter crate + frontend dir. The host's `build.rs` discovers plugins, emits Rust registry + frontend tab registry (`src/generated/plugin-tabs.ts`) + per-plugin TS wrappers — without edits to host source, router enums, command match arms, or tab switch statements.
  - AC-1.1: `plugin.toml` schema enforced. Required fields: `name`, `version`, `type` (`tab` for MVP), `command_bin` (sidecar binary name), `frontend` (relative path to frontend entry), `permissions[]`, `required_apis[]`, `db_namespace`, `migrations_path`. Manifest uniqueness checks enforced: duplicate `plugin_id`, duplicate `db_namespace`, or duplicate `command_bin` across plugins fails the build.
    - Positive: a valid manifest registers the plugin and surfaces its tab at app launch with the declared permission set visible in the registry.
    - Negative: a manifest missing any required field fails the build (`build.rs` aborts with typed error naming the field); a manifest declaring an unknown `required_apis` value fails the build; two plugins declaring the same `db_namespace` fail the build with a typed error naming both plugin IDs.
  - AC-1.2: Typed IPC code-gen pipeline emits BOTH Rust registry AND frontend TS wrappers. Generated outputs land at known repo-local paths that are gitignored and regenerated during each build (NOT checked-in). Clean-on-build avoids stale artifacts.
    - Positive: TS calls go through `plugins.<id>.commands.<commandName>(args, capability)` style generated functions; `tsc` catches wrong arg types at compile time. Removing a plugin and rebuilding produces a TS compile error for any code still calling its removed commands. Per-command permission metadata is generated alongside (single source of truth: same Rust `#[command(permissions=["user.email"])]` annotation drives both the dispatcher's gate and the TS wrapper's docstring).
    - Negative: raw `invoke('plugin.<id>.<command>', args)` calls are not used anywhere in plugin frontend code (enforced by lint rule or convention check).
  - AC-1.3: Each plugin owns its own SQLite database file at `${APP_DATA}/plugins/<id>/state.sqlite` with WAL journaling, `busy_timeout` configured (default 5000ms), advisory-lock-guarded migrations to prevent concurrent migration runs from sibling sidecar copies.
    - Positive: under the N×M lifecycle, multiple sibling sidecar copies of the SAME plugin (e.g., 3 concurrent Gmail sidecars from 3 claude tabs) all opening `${APP_DATA}/plugins/gmail/state.sqlite` complete a 100-operation read/write stress test without races or avoidable `database is locked` failures; only ONE sidecar's migration runs to completion on startup (advisory lock), others wait then resume against the migrated schema.
    - Negative: the host exposes no API allowing plugin A to open or query plugin B's `state.sqlite` file; cross-plugin SQL is impossible.
  - AC-1.4: Permission gating is enforced at the Rust IPC dispatcher (mandatory; warning-only is forbidden). The dispatcher rejects any call whose target command requires a permission the calling plugin's manifest does not declare. Per-command permission metadata is single-sourced from Rust annotations (re-exported into the TS wrapper docs).
    - Positive: a plugin manifest with `permissions: ["user.email"]` can invoke Gmail-scoped commands; the dispatcher logs (caller plugin_id, target command, permission match) with correlation IDs.
    - Negative: a plugin without `user.email` calling a Gmail-scoped command receives a typed `PermissionDenied { caller_plugin_id, missing_permission, requested_command }` error before the target ever executes.
  - AC-1.5: Caller identity via `PluginCapability` is a **branded opaque handle** (an unguessable server-issued nonce or signed token), held only in the plugin component's React closure (NOT on `window`, NOT in `localStorage`/`sessionStorage`/`IndexedDB`, NOT serializable to user-readable JSON). Authoritative capability state lives in Rust's `MountRegistry` keyed by `(plugin_id, mount_id)`. The handle is required on every permissioned IPC call; the dispatcher validates the handle against the registry on every call.
    - Positive: a permissioned wrapper invoked from plugin A's component (carrying A's handle) succeeds. Calling the same wrapper from plugin B's component (carrying B's handle, distinct nonce) fails with `PermissionDenied` because the Rust dispatcher looks up `(plugin_id from handle, mount_id from handle)` and rejects mismatches.
    - Negative: calls carrying a forged handle (random bytes), an expired handle (post-unmount, registry entry removed), a handle bound to a different `(plugin_id, mount_id)` pair, or no handle at all are rejected with typed errors (`CapabilityInvalid`, `CapabilityExpired`, `CapabilityMismatched`, `CapabilityMissing`). In-flight IPC calls during a remount complete atomically against the old handle OR get rejected with `CapabilityExpired` (no half-states with mixed pre/post-rotation state).
  - AC-1.6: Plugin sidecar lifecycle: host-owned UI sidecars vs per-claude-tab sidecars have distinct identities (separate `client_id`s like `host_ui:<plugin_id>` vs `claude:<tab_id>:<plugin_id>`; separate log prefixes; separate capability scopes). Each sidecar handles a `Shutdown` IPC gracefully and is force-killed after a 5-second timeout.
    - Positive: spawning a per-claude sidecar from a fresh `claude` tab creates a sidecar with a unique `client_id` visible in logs; sending `Shutdown` results in clean exit within 5s.
    - Negative: a sidecar that ignores `Shutdown` is SIGKILL'd at 5s; orphaned sidecars do not persist in `ps aux` after host process exit.
  - AC-1.7: Sidecar crash triggers auto-restart with exponential backoff (1s → 2s → 4s → … → 60s upper cap; resets after 5 minutes of stable uptime). The affected plugin's tab shows an error state during restart with a manual retry button. Other plugins are unaffected.
    - Positive: `kill -9 <gmail sidecar pid>` triggers restart attempts logged with correlation IDs at +1s, +2s, +4s; Papers tab continues to render normally throughout.
    - Negative: the Gmail tab does not silently disappear or render a white screen; orchestrator's `claude` sees the disconnected MCP server and surfaces an error in that tool call but does not crash.

- **AC-2: MCP Stdio Server Discipline + Claude Wiring.**
  - AC-2.1: Every plugin sidecar exposes an MCP server over stdio with correct framing. Logs are written ONLY to stderr or a per-process log file at `${APP_DATA}/logs/<plugin_id>/<client_id>.log`. Stdout carries protocol bytes only.
    - Positive: piping a sidecar's stdout into a strict MCP parser during a 60-second log-heavy test run produces zero framing/deserialization errors.
    - Negative: introducing a `println!`/`console.log` into sidecar code during dev causes test failure (lint rule + integration test catch it).
  - AC-2.2: Per-claude-tab MCP config generator. When a tab launches `claude`, the host writes an ephemeral MCP config file (e.g. `${APP_DATA}/claude-mcp-configs/<tab_id>.json`) listing exactly one sidecar invocation per plugin for that tab, then passes its path to `claude` via the documented CLI mechanism. Tab/workspace identity is passed via sidecar args or env vars so each sidecar knows which tab spawned it.
    - Positive: opening a new Terminal Mesh tab and running `claude` results in `claude` having access to all MVP plugin MCP servers; sidecar logs show the spawning `tab_id` and `workspace_path`.
    - Negative: closing the tab removes the config file; orphan config files older than the host's launch timestamp are cleaned at next startup.
  - AC-2.3: `claude` CLI PATH discovery does NOT rely on GUI app shell inheritance. On first launch, the host probes (a) known install paths (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`), (b) the user's login-shell PATH via `bash -lc 'command -v claude'`. The discovered path is persisted in app config. The orchestrator's onboarding state reflects detection result.
    - Positive: launching the app via Finder (no inherited terminal PATH) finds `/opt/homebrew/bin/claude` on Apple Silicon Macs.
    - Negative: if `claude` is absent in all probed paths, the orchestrator shows a Chinese onboarding card listing searched paths + a "选择 claude 路径" file picker — and does NOT spawn empty sidecar processes.

- **AC-3: Orchestrator is a privileged single-instance host tab.**
  - AC-3.1: Top-left fixed tab position with distinct icon. The host enforces single-instance semantics: opening "Orchestrator" while one already exists focuses the existing tab.
    - Positive: clicking the orchestrator slot twice never spawns two tabs.
    - Negative: closing the orchestrator and reopening it spawns a fresh tab with rotated capabilities.
  - AC-3.2: On open, the orchestrator's PTY auto-launches `claude` with the platform's MCP config preconfigured (one sidecar per MVP plugin); if `claude` is unavailable, it shows the onboarding card (per AC-2.3).
    - Positive: opening the orchestrator from a cold start shows the `claude` prompt with MCP servers connected (verifiable via `claude` tool listing).
    - Negative: opening with `claude` absent does NOT fork-exec any plugin sidecars.
  - AC-3.3: Cross-tab read capability for the orchestrator. The orchestrator's `PluginCapability` for the Terminal Mesh MCP server includes a `cross_tab_read=true` permission flag; this flag is granted ONLY by the Rust dispatcher and is never present in any object visible to frontend serialization (it lives in the MountRegistry alongside the orchestrator's capability handle).
    - Positive: orchestrator's claude can call `terminal_mesh.read_scrollback(tab_id=X)` for any tab and receive a bounded response (max-bytes documented per call).
    - Negative: a regular tab's claude calling the same tool for a different `tab_id` receives `PermissionDenied`. Serializing any object reachable from the frontend (window, React state, capability handle string itself) reveals NO `cross_tab_read` flag — the flag exists only in Rust state.
  - AC-3.4: Gmail send via orchestrator's OAuth delegation. The orchestrator's claude has NO Gmail credentials of its own; it requests `gmail.send` via the Gmail sidecar's MCP tool, which (after host confirm-on-write approval) reads the token from Stronghold and dispatches the send.
    - Positive: orchestrator can complete a Gmail send via claude → MCP call → confirm-on-write modal → user approves → Gmail sidecar uses Stronghold-stored token → API call. Token is at NO point passed through orchestrator's process boundary, claude's stdin/stdout, MCP config file, environment variables, or logs (verified by audit of all these surfaces).
    - Negative: code-path assertion — orchestrator declares no `secrets.gmail` permission in its capability scope; the orchestrator has no path to Stronghold's Gmail entries. (Memory scanning is offered as an OPTIONAL diagnostic, NOT a required AC test.)
  - AC-3.5: Orchestrator's claude task-complete events trigger a notification with a semantic summary (e.g., "claude: 标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐") rather than "exit 0".
    - Positive: a long claude task completing emits one tray entry + one native notification containing claude's `task_summary` (provided via OSC sequence or MCP notification).
    - Negative: a raw `bash` exit in the orchestrator triggers the regular completion notification, NOT the semantic-summary path.

- **AC-4: Terminal Mesh + Workspaces.**
  - AC-4.1: At least **4 concurrent PTY sessions** can be open simultaneously [HARD, user-confirmed]. Each runs a Tokio actor with a **1 MB bounded ring buffer per PTY** [HARD, user-confirmed], handling UTF-8 multi-byte and ANSI escape sequence boundaries safely (no truncation mid-codepoint or mid-escape).
    - Positive: spawn 4 PTYs running a portable synthetic fast-output command (e.g. `yes | head -c 100M` or `seq 1 1000000`); all four render concurrently in xterm.js without visible blocking or garbled output. Spawning a 5th PTY succeeds.
    - Negative: closing a PTY tab while a command runs terminates the child process cleanly (verified via `ps aux` showing no orphan with the workspace's cwd); a 10MB burst into a single PTY's stdout does not OOM the host (ring overflow coalesces oldest bytes per documented policy).
  - AC-4.2: PTY supports stdin / SIGWINCH resize / exit-status reporting / cwd / env at spawn.
    - Positive: typing reaches the child; viewport resize propagates SIGWINCH; child exit code surfaces in the tab's status bar; a PTY spawned with `cwd=~/AgentPlatform/workspaces/foo` reports that cwd via `pwd`.
    - Negative: spawning with an invalid shell path returns a typed error and renders an in-tab error state without crashing the app.
  - AC-4.3: Each Terminal Mesh tab is bound to exactly ONE workspace. Workspaces live at `~/AgentPlatform/workspaces/<name>/` (user-visible, accessible by Cursor/VS Code) OR point to a user-selected existing directory.
    - Positive: creating a new workspace named "debug-arxiv" creates `~/AgentPlatform/workspaces/debug-arxiv/`; picking an existing directory associates the tab with it without copying or modifying it.
    - Negative: workspace names containing forbidden filesystem characters are rejected at the modal level with a typed error; the platform never auto-creates `.git/` in the workspace dir.
  - AC-4.4: Workspaces are persistent across app restart. One workspace = AT MOST one tab at a time, enforced by canonical-path resolution (symlinks resolved, case-insensitive paths normalized on macOS).
    - Positive: closing the app and reopening it shows previously-created workspaces in the switcher; reopening a workspace restores its tab and its `.claude/` conversation context.
    - Negative: clicking a workspace already open in another tab focuses that tab (no duplicate); creating two workspaces whose user-selected dirs resolve to the same canonical path is rejected at creation with a typed error.
  - AC-4.5: Workspace switcher modal triggered by the tab strip's `+` button. Two affordances: "Create new" (name input + path source: auto OR pick existing dir) and "Pick recent" (scrollable list). Each list row shows: name + last-used timestamp + created timestamp + claude conversation rounds count (extracted from `.claude/`).
    - Positive: clicking `+` opens the modal; typing a name + clicking Create produces a new tab with the named workspace.
    - Negative: the modal is not stacked on top of a confirm-on-write modal (modal coordinator enforces single-active-modal); cancelling the modal closes it cleanly without creating any artifacts.
  - AC-4.6: Per-workspace IDE handoff. Each workspace row (in switcher AND in active tab's settings) exposes "Open in Cursor" (configurable to VS Code / Zed / other) and "Reveal in Finder".
    - Positive: clicking "Open in Cursor" invokes `cursor <workspace_path>` (or configured alternative).
    - Negative: if the configured IDE command is not in PATH, the button shows a typed error toast instead of failing silently.

- **AC-5: Gmail Plugin (multi-account, OAuth, incremental sync, modify operations).**
  - AC-5.1: At least **2 Gmail accounts** added independently [HARD, user-confirmed]. OAuth via system browser loopback. Per-account state isolation in Stronghold and SQLite.
    - Positive: adding two accounts produces two switchable identities; each shows its own inbox; revoking account A externally produces a typed re-auth prompt for A without affecting B.
    - Negative: browser-launch failure during OAuth produces a typed error with a "重试" option; duplicate-account-add detection rejects re-adding the same `account_id` with a typed error.
  - AC-5.2: OAuth refresh tokens stored ONLY in `tauri-plugin-stronghold`, keyed by `(plugin_id, account_id)`. Access tokens stay in process memory.
    - Positive: scanning all plugin SQLite files post-OAuth finds zero refresh-token strings; scanning WebView storage finds zero tokens.
    - Negative: removing an account triggers Stronghold key deletion + SQLite metadata cleanup; no token survives account removal.
  - AC-5.3: Incremental sync via Gmail `historyId`. `historyNotFound` triggers controlled resync with visible progress. `429 Too Many Requests` triggers exponential backoff with documented ceiling: cap of 5 minutes between retries; reset on successful response.
    - Positive: app restart resumes from last-stored `historyId`; only new messages fetched. A `429` response triggers an exponential backoff visible as "syncing slower" indicator; ceiling at 5min between retries.
    - Negative: an invalid `historyId` (`historyNotFound`) triggers controlled resync, not a crash; backoff never effectively stalls (5min ceiling).
  - AC-5.4: Stronghold setup recovery. Interrupted master-password setup can be resumed or reset cleanly without leaving partially-authorized accounts.
    - Positive: killing the app during initial Stronghold setup, then relaunching, presents either "继续设置" or "重置 vault" with explanation of consequences.
    - Negative: no plaintext token appears in any file after an interrupted setup; orphaned partial-OAuth state in SQLite is detected and cleaned at next startup.
  - AC-5.5: Send via Gmail plugin requires confirm-on-write approval. Operation IDs ensure idempotency on sidecar retries.
    - Positive: a `gmail.send` call from any claude tab pops a confirm modal showing recipient + subject + body preview + Confirm/Cancel; confirming dispatches exactly one send; sidecar retry with the same operation ID does NOT cause a duplicate send.
    - Negative: cancelling the confirm returns a typed `Rejected` error to claude; no Gmail API call is made.
  - AC-5.6: Modify operations (label / archive / mark-read / mark-unread / trash) gated by confirm-on-write. No permanent delete (`gmail.delete` scope NOT requested in MVP).
    - Positive: a `gmail.label`, `gmail.archive`, `gmail.mark_read`, `gmail.mark_unread`, or `gmail.trash` call from any claude tab pops a confirm modal with action description + target message metadata + Confirm/Cancel; confirming executes the modify; cancelling returns `Rejected`.
    - Negative: `gmail.delete` (permanent delete) is NOT in the MVP MCP tool list — code lookup confirms no such tool is registered.
  - AC-5.7: Attachments listed inline, opened on-demand. No auto-download, no indexing. Attachment temp files (opened for system-app launch) cleaned up within 60 seconds of opening or at app exit (whichever is sooner).
    - Positive: clicking an attachment opens it via macOS default app (`open` shell-out or `NSWorkspace`).
    - Negative: temp files from attachment opens are absent from disk within 60s; closing the inbox tab does not leave attachment temp files lingering past the 60s window. Gmail tab UI does NOT include compose/send buttons (those affordances are Phase 2; send happens only via claude in MVP).

- **AC-6: Papers Plugin (Zotero three-mode + arXiv).**
  - AC-6.1: Zotero access in priority order: (1) **Live** — HTTP at `localhost:23119` when Zotero is running and the local API is enabled; (2) **SQLite read-only fallback** — `?mode=ro&immutable=1` on `zotero.sqlite` when Zotero is closed but the file is readable; (3) **App-owned cached snapshot** — the plugin's own materialized cache when both above fail. Each mode has a visible status indicator. Zotero SQLite path discovery: default macOS path (`~/Zotero/zotero.sqlite`); user can override in plugin settings.
    - Positive: with Zotero running, Live path is taken; closing Zotero falls back to SQLite-ro within one health-check cycle; deleting/locking `zotero.sqlite` falls back to cached snapshot with a "offline — using snapshot from <timestamp>" banner; user-configured Zotero path is honored.
    - Negative: the plugin never opens `zotero.sqlite` in read-write mode; a corrupted Zotero SQLite file (catch deserialization error) produces cached-snapshot fallback rather than crashing.
  - AC-6.2: arXiv via `arxiv-rs`; results merged with Zotero items and deduplicated by DOI / arXiv ID.
    - Positive: an arXiv item already in Zotero by DOI appears once with an "in-library" badge.
    - Negative: items with same author/title prefix but distinct DOIs are NOT collapsed.
  - AC-6.3: Rule-based recommendations only (categories + Zotero tag affinity). No personalized ranking.
    - Positive: subscribing to `cs.LG` produces a fresh list filtered by current arXiv submissions.
    - Negative: removing a subscription stops new items appearing; no implicit "remember-my-clicks" behavior.
  - AC-6.4: Polite caching: target **~30 minute minimum refresh interval per query** [DIRECTIONAL]. Manual refresh respects a short rate guard.
    - Positive: clicking refresh twice rapidly produces only one network call.
    - Negative: programmatic poll rate per query does not exceed configured minimum by more than one order of magnitude.

- **AC-7: Confirm-on-Write Surface.**
  - AC-7.1: Every plugin-mediated write originating from a claude tab triggers a host-owned confirm modal showing action description + payload preview + Confirm/Cancel.
    - Positive: a `gmail.label` call shows a modal with "添加标签 'Important' 到邮件 X"; confirming labels the email; cancelling returns `Rejected`.
    - Negative: a read-only call (e.g., `gmail.list_inbox`) does NOT trigger a confirm modal.
  - AC-7.2: Concurrent write requests across tabs are arbitrated by a host-owned global queue, not stacked modals.
    - Positive: two simultaneous `gmail.label` calls from two tabs are serialized; the second modal appears after the first is dismissed; queued requests show a "pending approval" indicator in their originating tab.
    - Negative: there is never more than one open confirm modal at a time across the app.
  - AC-7.3: Approval carries an idempotent operation ID. A sidecar retrying after approval (e.g., transient network error) reuses the same operation ID to avoid duplicate writes.
    - Positive: a sidecar retry with the same operation ID against an already-completed approval returns the cached result, not a fresh write.
    - Negative: a retry with a different operation ID is treated as a new request requiring fresh approval.
  - AC-7.4: Terminal stdin confirmation (claude writing into another tab's PTY via orchestrator) pauses the write but does NOT block the destination PTY's output stream, resize events, or exit observation.
    - Positive: while a stdin confirm modal is open, the destination PTY's output continues rendering; the PTY can be resized; child exit is observed and surfaced.
    - Negative: cancelling the stdin confirm does not corrupt the destination PTY's state.
  - AC-7.5: Scope honesty. Confirm-on-write covers plugin-mediated writes (Gmail send/label/etc., Papers add/edit, Terminal stdin from orchestrator). Raw filesystem writes by claude inside a workspace's own dir are NOT intercepted in MVP. This boundary is documented in the orchestrator onboarding card and project README.
    - Positive: a `gmail.send` IS gated.
    - Negative: claude writing `notes.md` in its own workspace via `Edit`/`Write` tool is NOT gated (and is not claimed to be in any docs or UI).

- **AC-8: Notification Surface ("灵动岛" analog).**
  - AC-8.1: Native macOS notification + menubar tray window (`TrayBottomCenter`) both fire on terminal completion. Notification permission requested on first event that would trigger a notification (lazy, not at app startup).
    - Positive: `sleep 10 && echo done` produces one banner + one tray entry; first such event triggers the permission prompt.
    - Negative: a tight loop of fast-completing commands produces at most one notification per `(plugin_id, terminal_id, event_kind)` per ~2s dedup window.
  - AC-8.2: Default needs-attention triggers: child exit 0, nonzero exit, OSC agent marker, prompt-waiting heuristic (where reliably detectable). Stderr-burst is NOT a default trigger.
    - Positive: OSC agent marker triggers a needs-attention tray entry.
    - Negative: writing a large blob to stderr does NOT trigger an alert.
  - AC-8.3: Orchestrator's claude task-complete fires a notification with a claude-generated semantic summary.
    - Positive: a multi-step claude task surfaces a summary like "标记 3 封邮件为已读".
    - Negative: a non-claude command in the orchestrator triggers the regular completion event, not the semantic-summary path.
  - AC-8.4: Notification permission denial is graceful.
    - Positive: with notifications denied, an in-app banner explains the fallback; tray window continues to drive event surfacing.
    - Negative: denied permission does NOT cause a runtime panic or repeated re-prompts.

- **AC-9: Application Shell Robustness + First-Run Bootstrap + Quit Sequencing.**
  - AC-9.1: First-run bootstrap creates required dirs: `~/AgentPlatform/`, `~/AgentPlatform/workspaces/`, `${APP_DATA}/plugins/<id>/`, `${APP_DATA}/logs/`, `${APP_DATA}/claude-mcp-configs/`. Permission failures surface a user-readable error.
    - Positive: first launch on a fresh user account creates all required dirs and shows the empty workspace switcher.
    - Negative: a permission failure (e.g., read-only home dir) shows a Chinese error card with the offending path and remediation hint instead of crashing.
  - AC-9.2: Cold start with no plugin sidecars built/bundled shows a dev-diagnostics page listing missing binaries and a hint to run the build command.
    - Positive: in a dev checkout where the build has not produced sidecar binaries, the app launches and shows "缺失 plugin 二进制：…" with the list of expected binaries.
    - Negative: production builds with all sidecars bundled never show this page.
  - AC-9.3: SQLite migration failure for one plugin blocks that plugin's mount, shows an error state in its tab, and leaves other plugins unaffected.
    - Positive: a simulated Gmail migration error keeps Papers + Terminal Mesh running; the Gmail tab shows error state with a "重试迁移" button.
    - Negative: migration failure does NOT crash the host process.
  - AC-9.4: App quit sequence: stop accepting new writes → resolve/cancel pending confirm modals → terminate PTYs (SIGTERM + grace + SIGKILL) → send graceful Shutdown to all sidecars → force-kill after timeout. No orphaned sidecars or PTY children remain after **normal quit**. **OS force-quit / `kill -9` of the host is acknowledged as unable to guarantee cleanup hooks**; the host MUST detect and reap stale app-owned process records at next startup where possible.
    - Positive: `lsof` and `ps aux` after a normal quit show no orphan processes attributable to the app. On next launch after a force-quit, startup cleanup scans `${APP_DATA}/sidecar-state/` for stale PID records and attempts to reap them.
    - Negative: normal quit (Cmd+Q / app menu Quit) never leaves orphans. The plan and onboarding docs do NOT claim cleanup hooks fire on force-quit / SIGKILL.
  - AC-9.5: Structured JSON logs per process with correlation IDs (`tab_id`, `plugin_id`, `request_id`). OAuth tokens and email bodies are redacted in logs.
    - Positive: a request/event log entry carries correlation IDs joinable across host + sidecar logs.
    - Negative: grepping logs for token-shaped strings (long base64-ish near `refresh_token` keys) or for email-body content returns zero matches.

## Path Boundaries

> Lower Bound = minimum implementation that still passes ALL ACs. Upper Bound = polished MVP. Slack lives in UI polish, error-message richness, observability depth — NOT in AC-required behaviors.

### Upper Bound (Maximum Acceptable Scope)
- Plugin contract: full manifest schema with optional metadata (icons, sort order); polished error UI per plugin; lint rule banning raw `invoke('plugin.…')`; `cargo make`-style build orchestration; manifest uniqueness checks.
- MCP wiring: per-tab MCP config files with realtime cleanup AND startup GC; sidecar log files rotated by size; structured JSON dev logs with correlation IDs visible in dev tools.
- Orchestrator: polished Chinese onboarding card with `claude` path picker; semantic-summary notification with action-count breakdown; cross-tab read bounded byte cap visible in settings.
- Terminal Mesh: ≥4 PTYs (HARD), 1 MB ring (HARD), UTF-8/ANSI-safe boundary handling, polished tab-close animations, drag-to-reorder, switcher modal with debounced search.
- Gmail: multi-account read + modify (label/archive/mark-read/mark-unread/trash) + send. Polished compose UI in confirm-on-write modal (diff against current state). Attachment list/open-on-demand. Polished re-auth UX.
- Papers: full three-mode Zotero with visible mode status; arXiv polite caching with per-query refresh times shown; in-library badge with hover tooltip.
- Confirm-on-write: global queue with pending-approval count badge in tab strip; idempotent operation IDs surfaced in approval modal for debug; single-action approval only (no batch).
- Notifications: native + tray + semantic summaries; permission-denial fallback banner; dedup arbiter inspectable in dev tools.
- Application shell: structured JSON logs with redaction; quit sequencing with progress indicator; Chinese UI strings throughout with `i18n` key layer ready for Phase 2.
- Packaging: macOS signed/notarized `.app` is STRETCH.

### Lower Bound (Minimum Acceptable Scope — still passes ALL ACs)
- Plugin contract: manifest enforced (+ uniqueness checks), codegen pipeline working with gitignored generated dirs, per-plugin DB files with WAL + busy_timeout + advisory-lock-guarded migrations, mandatory permission gating, branded-handle `PluginCapability` validated against MountRegistry, distinct host-UI vs per-claude sidecar identities, auto-restart with exponential backoff (all AC-1 sub-criteria required).
- MCP wiring: stdio framing with logs to stderr/files; per-tab MCP config generator works (orphan cleanup at next startup at minimum); `claude` PATH discovery probes known paths + login-shell PATH (AC-2 required).
- Orchestrator: top-left fixed single-instance auto-launches claude; cross-tab read works for orchestrator only with `cross_tab_read` flag in Rust state; Gmail send via OAuth delegation works without exposing tokens; semantic-summary notification works (AC-3 required).
- Terminal Mesh: ≥4 concurrent PTYs HARD; 1 MB ring HARD; UTF-8/ANSI-safe slicing; workspace creation + persistence + canonical-path uniqueness; switcher modal; Open in Cursor + Reveal in Finder.
- Gmail: ≥2 accounts HARD; OAuth via system browser; Stronghold-only tokens; historyId incremental sync with 5-min backoff ceiling; Stronghold interrupted-setup recovery; send + modify ops (label/archive/mark-read/unread/trash) via confirm-on-write with operation IDs; attachment list/open-on-demand; NO compose/send UI buttons.
- Papers: all three Zotero modes (Live/SQLite-ro/cached snapshot) with default + user-overridable path; DOI/arXiv-ID dedup; rule-based recommendations; polite caching (~30 min DIRECTIONAL).
- Confirm-on-write: every plugin-mediated write gated; global queue prevents stacked modals; operation IDs for idempotency; terminal stdin pause without output block; scope honesty documented.
- Notifications: native + tray on completion (with lazy first-event permission request); dedup keyed by `(plugin_id, terminal_id, event_kind)` ~2s; semantic-summary path for orchestrator's claude; permission-denial graceful fallback.
- Application shell: first-run bootstrap; dev-diagnostics page when no plugins built; plugin migration failure plugin-scoped; normal-quit sequencing with no orphans + startup-time stale-process reaping; structured logs with correlation IDs and redaction.
- Chinese UI strings (DEC-11 locked).

### Allowed Choices
- **Can use**: Tauri v2; React 18+ / TypeScript; Rust (stable, 2024 edition); `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold`, `rusqlite` (WAL + busy_timeout); `google-gmail1` (Gmail REST); `arxiv-rs`; Zotero local HTTP API + `zotero.sqlite` read-only.
- **Implementation-decision items (NOT user decisions — deferred to coding phase)**: MCP Rust SDK (`rmcp` vs hand-written stdio); type-codegen (`ts-rs` vs `specta`); migration runner (`refinery` vs `sqlx::migrate!`); React state (Zustand vs Context+reducer); log shape (JSON via `tracing` vs custom); SQLite access pattern (pool per sidecar vs actor-owned connection).
- **Cannot use**: Electron; pure-web SPA; SwiftUI / AppKit-native; tmux-as-system-of-record; notebook-cell UI; in-process plugins; raw `invoke('plugin.…')` strings in plugin code; hand-maintained plugin enumeration; encrypted SQLite (Stronghold for secrets only); community-MCP-server forks (self-written sidecars only); macOS App Sandbox / per-plugin entitlements for MVP (OS process boundary only); send/reply UI buttons in Gmail tab (Phase 2 — send via claude only in MVP); per-plugin sidecar dedup proxy; `gmail.delete` scope (Phase 2).

## Feasibility Hints and Suggestions

### Conceptual Approach

1. **Host skeleton** — Tauri v2 + React/TS shell. Tab strip with fixed orchestrator slot at top-left and a `+` for workspace tabs. Backend exposes `PluginHost` trait + IPC dispatcher.
2. **Plugin contract** — `plugins/<id>/plugin.toml` + Rust adapter crate + frontend dir. `build.rs` discovers plugins, emits Rust registry, frontend tab registry, per-plugin TS wrappers. Generated dirs gitignored (`src-tauri/src/generated/`, `src/generated/`) and regenerated from clean each build.
3. **Typed IPC codegen** — `ts-rs` or `specta` (impl-decide during coding). Rust command/event/error types are SoT. Per-command permission metadata generated from `#[command(permissions=["..."])]` annotations into BOTH dispatcher gating and TS wrapper docstrings.
4. **PluginCapability (branded handle)** — host issues a server-side nonce (e.g., random 128-bit token) on mount; passes it through Tauri IPC to a React `usePluginCapability` context provider; React closure holds it; generated wrappers carry it as a hidden first argument. Rust `MountRegistry` keyed by `(plugin_id, mount_id)` stores `{handle_nonce, permissions, cross_tab_read_flag}`. Validation on every IPC: dispatcher looks up the registry, rejects forged/expired/mismatched/missing handles.
5. **MCP stdio sidecars** — pick `rmcp` or hand-written. Stdout is protocol-only; logs to `${APP_DATA}/logs/<plugin_id>/<client_id>.log`. Lint/CI rule catches `println!`/`console.log` in sidecar code. Sidecar `Shutdown` IPC → flush state → exit; host waits 5s then SIGKILL.
6. **claude wiring** — host generates `${APP_DATA}/claude-mcp-configs/<tab_id>.json` listing one sidecar invocation per plugin (`{"command": "/path/to/gmail-plugin", "args": ["--client-id", "claude:<tab_id>:gmail", "--workspace", "<workspace_path>"]}`). Tab launches `claude --mcp-config <path>`. Cleanup on close + GC at startup.
7. **claude PATH discovery** — on first launch probe `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`; fall back to `bash -lc 'command -v claude'`; persist result. Orchestrator onboarding card if missing.
8. **Per-plugin SQLite** — each plugin opens `${APP_DATA}/plugins/<id>/state.sqlite` with `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;`. Use `refinery` or `sqlx::migrate!`. Migration acquires an advisory lock (e.g., file lock at `state.sqlite.migration-lock`) so only one sibling sidecar copy runs migrations; others wait then resume.
9. **Stronghold** — refresh tokens at `(plugin_id, account_id)` keys. Master password setup with resume/reset on interrupt (marker file at `${APP_DATA}/stronghold-state/setup.marker`).
10. **Terminal Mesh** — `portable-pty` Tokio actor with `VecDeque<u8>` 1 MB ring + overflow drop-oldest-with-coalesce. UTF-8/ANSI-safe: scanner finds safe boundaries before slicing. xterm.js subscribes via typed events.
11. **Workspace persistence** — central registry at `${APP_DATA}/workspaces.json` (canonical paths, names, timestamps). Optional `.workspace.json` in each workspace dir overrides metadata. Conversation rounds = parse `.claude/` files best-effort.
12. **Confirm-on-write queue** — host-owned arbiter; sidecars send `RequestApproval { operation_id, action, payload }`; arbiter queues, opens one modal at a time, replies `Approved {operation_id}` or `Rejected`. Sidecar holds `oneshot` awaiting reply.
13. **Notification surface** — host subscribes to plugin events with `notify` capability declared. Dedup keyed by `(plugin_id, terminal_id, event_kind)` over ~2s. Tray window `TrayBottomCenter` via positioner; native via tauri-plugin-notification. Permission request lazy on first event.
14. **App quit** — `AppQuitCoordinator`: stop write-acceptance → resolve modals → SIGTERM PTYs (5s grace, then SIGKILL) → graceful sidecar Shutdown → SIGKILL after timeout. Startup cleanup scans `${APP_DATA}/sidecar-state/` for stale records (force-quit recovery).

### Specification Deliverables (produced by `analyze`-tagged tasks)
- `docs/specs/plugin-contract.md`
- `docs/specs/mcp-sidecar.md`
- `docs/specs/claude-launch.md`
- `docs/specs/terminal-events.md`
- `docs/specs/confirm-on-write.md`
- `docs/specs/gmail-sync.md`

### Relevant References
- Tauri v2 architecture: https://v2.tauri.app/concept/architecture/
- Tauri v2 plugin & sidecar: https://v2.tauri.app/develop/plugins/, https://v2.tauri.app/develop/sidecar/
- MCP spec (2025-11-25): https://modelcontextprotocol.io/specification/2025-11-25
- Claude Code MCP: https://code.claude.com/docs/en/mcp
- xterm.js, portable-pty, google-gmail1, arxiv-rs (cited in draft)
- Terax AI (working precedent): https://github.com/crynta/terax-ai

## Dependencies and Sequence

### Milestones

1. **Plugin Contract Foundation**
   - Phase A: Tauri scaffold + React/TS shell + fixed-orchestrator-slot tab strip.
   - Phase B: Write `docs/specs/plugin-contract.md`.
   - Phase C: `build.rs` codegen pipeline.
   - Phase D: Generated Rust IPC dispatcher with structured JSON logging + correlation IDs (logging is foundational; built in alongside dispatcher per Codex round-1 suggestion).
   - Phase E: Per-plugin SQLite framework with WAL + busy_timeout + advisory-lock-guarded migrations.
   - Phase F: tauri-plugin-stronghold integration with setup/resume/reset.
   - Phase G: Frontend lifecycle hooks + branded-handle PluginCapability + host error boundary.
   - Phase H: First-run bootstrap + dev-diagnostics page.

2. **MCP Sidecar Discipline + Claude Wiring**
   - Phase A: Write `docs/specs/mcp-sidecar.md`.
   - Phase B: MCP stdio framing helper (logs→stderr/files).
   - Phase C: Sidecar lifecycle manager (spawn/exp-backoff/Shutdown/distinct identities).
   - Phase D: Write `docs/specs/claude-launch.md`.
   - Phase E: claude PATH discovery + per-tab MCP config generator + onboarding card.

3. **Terminal Mesh + Workspaces**
   - Phase A: Write `docs/specs/terminal-events.md`.
   - Phase B: portable-pty Tokio actor with 1 MB ring + UTF-8/ANSI-safe slicing.
   - Phase C: xterm.js multi-tab frontend with ≥4 PTY stress test.
   - Phase D: Workspace storage layer + canonical-path uniqueness.
   - Phase E: Workspace switcher modal.
   - Phase F: Open in Cursor / Reveal in Finder.

4. **Orchestrator**
   - Phase A: Top-left fixed single-instance host tab.
   - Phase B: Cross-tab read privileged capability path.
   - Phase C: Gmail send via OAuth delegation pathway.
   - Phase D: Notification surface (tauri-plugin-notification + positioner + dedup arbiter).
   - Phase E: Orchestrator semantic-summary notification.

5. **Confirm-on-Write + Write Queue**
   - Phase A: Write `docs/specs/confirm-on-write.md`.
   - Phase B: Global confirm-on-write queue arbiter + modal UI.
   - Phase C: Terminal stdin confirm-on-write wiring.

6. **Gmail Plugin**
   - Phase A: Write `docs/specs/gmail-sync.md`.
   - Phase B: Gmail sidecar scaffold + OAuth loopback flow.
   - Phase C: Multi-account Stronghold storage + interrupted-setup recovery.
   - Phase D: Incremental sync engine + 429 backoff with 5-min ceiling.
   - Phase E: Send via confirm-on-write with operation IDs.
   - Phase F: Modify operations (label/archive/mark-read/unread/trash) via confirm-on-write.
   - Phase G: Inbox UI (Chinese strings) + account switcher + attachment list/open-on-demand; explicitly no compose/send UI buttons.

7. **Papers Plugin**
   - Phase A: Papers sidecar scaffold.
   - Phase B: Zotero three-mode access client (with user-overridable SQLite path).
   - Phase C: arxiv-rs query engine + dedup + rule-based recommendations + polite caching.
   - Phase D: Papers tab UI (Chinese strings) with merged list + offline indicator.

8. **Robustness + Quit**
   - Phase A: SQLite migration-failure plugin-scoped recovery UI (depends on M1-E).
   - Phase B: AppQuitCoordinator + startup stale-process reaping for force-quit recovery.

Relative dependencies:
- M1 substantially complete before M2 starts.
- M2 functional before M3-C orchestrator integration in M4.
- M3 + M4 mostly parallel after M2.
- M5 functional before M6-E (Gmail send) and M4-C orchestrator delegation.
- M6 + M7 parallel after M5.
- M8 runs alongside throughout; finalizing at the end.

## Task Breakdown

| Task ID | Description | Target AC | Tag | Depends On |
|---------|-------------|-----------|-----|------------|
| task1 | Scaffold Tauri v2 + React/TS shell with fixed-orchestrator-slot tab strip + first-run bootstrap | AC-9.1 | coding | - |
| task2 | Write `docs/specs/plugin-contract.md` (manifest schema with uniqueness checks, dispatcher rules, branded-handle PluginCapability lifecycle, permission enum, codegen contract with per-command permission metadata) | AC-1.1, AC-1.4, AC-1.5 | analyze | task1 |
| task3 | `build.rs` codegen pipeline emitting Rust registry + frontend tab registry + per-plugin TS wrappers + per-command permission metadata into gitignored generated dirs; clean-rebuild semantics | AC-1.1, AC-1.2 | coding | task2 |
| task4 | Generated Rust IPC dispatcher with permission gating + branded-handle PluginCapability validation against MountRegistry by `(plugin_id, mount_id)` + structured JSON logs with correlation IDs (`tab_id`, `plugin_id`, `request_id`, OAuth/email-body redaction) | AC-1.4, AC-1.5, AC-9.5 | coding | task3 |
| task5 | Per-plugin SQLite framework (WAL, busy_timeout=5000, per-plugin migration runner with advisory file-lock to coordinate sibling sidecar copies of the same plugin, plugin-scoped migration-failure recovery UI) | AC-1.3, AC-9.3 | coding | task1 |
| task6 | tauri-plugin-stronghold integration with master-password setup/resume/reset and interrupted-state detection via marker file at `${APP_DATA}/stronghold-state/setup.marker` | AC-5.2, AC-5.4 | coding | task1 |
| task7 | Frontend lifecycle hooks (`usePluginCapability` issuing branded handle from host; `onUnmount` rotating; `onError`) + host error boundary + capability-rotation race safety (in-flight IPC during rotation either completes atomically or returns `CapabilityExpired`) | AC-1.5, AC-9.3 | coding | task4 |
| task8 | Dev diagnostics page (cold start with no plugin sidecars built/bundled) | AC-9.2 | coding | task1 |
| task9 | Write `docs/specs/mcp-sidecar.md` (stdio framing, log channel rules with lint enforcement, shutdown contract with 5s timeout, client_id scheme distinguishing host-UI from per-claude) | AC-2.1, AC-1.6 | analyze | task1 |
| task10 | MCP stdio framing helper crate (logs→stderr/files; structured shutdown signaling; lint rule banning `println!`/`console.log` in sidecar code) | AC-2.1, AC-1.6 | coding | task9 |
| task11 | Sidecar lifecycle manager (spawn, exp-backoff auto-restart 1s→60s with 5-min stable-uptime reset, graceful Shutdown 5s timeout then SIGKILL, distinct host-UI vs per-claude client identities) | AC-1.7, AC-1.6 | coding | task10 |
| task12 | Write `docs/specs/claude-launch.md` (GUI app PATH discovery, MCP config file format with cleanup rules, claude version handling, onboarding card flow) | AC-2.3, AC-3.2 | analyze | task1 |
| task13 | `claude` PATH discovery (probe known paths + login-shell PATH) + per-tab MCP config generator at `${APP_DATA}/claude-mcp-configs/<tab_id>.json` with cleanup on tab close + startup GC + onboarding card if missing | AC-2.2, AC-2.3, AC-3.2 | coding | task12, task11 |
| task14 | Write `docs/specs/terminal-events.md` (event taxonomy + dedup keys + ring buffer overflow coalesce-on-overflow policy + UTF-8/ANSI boundary safety scanner) | AC-8.2 | analyze | task1 |
| task15 | portable-pty Tokio actor with 1 MB ring buffer (UTF-8/ANSI-safe slicing scanner, coalesce-on-overflow), cancellation, TerminalEvent stream | AC-4.1, AC-4.2 | coding | task14, task10 |
| task16 | xterm.js multi-tab frontend wiring; concurrent-PTY stress test (portable synthetic fast-output) verifying ≥4 PTYs simultaneously | AC-4.1 | coding | task15 |
| task17 | Workspace storage layer: `${APP_DATA}/workspaces.json` registry + canonical-path resolution (symlinks, case-insensitive macOS) + open-tab uniqueness lock + first-launch creation of `~/AgentPlatform/workspaces/` | AC-4.3, AC-4.4, AC-9.1 | coding | task5 |
| task18 | Workspace switcher modal (`+` button) with create / pick-existing + per-row metadata (name, last used, created, conversation rounds count from `.claude/`); modal coordinator preventing stacking with confirm-on-write modal | AC-4.5 | coding | task17, task7 |
| task19 | Open in Cursor + Reveal in Finder per workspace; configurable IDE command; typed error toast if IDE binary not in PATH | AC-4.6 | coding | task18 |
| task20 | Orchestrator host-side tab: top-left fixed, single-instance lock, auto-launch claude on open with MCP config preconfigured | AC-3.1, AC-3.2 | coding | task13, task16 |
| task21 | Cross-tab read privileged capability path: orchestrator's MountRegistry entry includes `cross_tab_read=true` flag in Rust state only (NOT serialized into any frontend-visible object); bounded read API with documented max-bytes | AC-3.3 | coding | task20, task15 |
| task22 | Notification surface: tauri-plugin-notification + tauri-plugin-positioner TrayBottomCenter + dedup arbiter keyed by `(plugin_id, terminal_id, event_kind)` with ~2s window + lazy permission request on first event + permission-denial in-app banner fallback | AC-8.1, AC-8.2, AC-8.4 | coding | task14, task20 |
| task23 | Orchestrator semantic-summary notification path (OSC sequence / MCP notification from claude → tray entry with task summary, distinguished from regular completion path) | AC-3.5, AC-8.3 | coding | task22, task20 |
| task24 | Write `docs/specs/confirm-on-write.md` (queue semantics, idempotent operation IDs, terminal-stdin pause-without-output-block, scope honesty about non-plugin-mediated writes) | AC-7.1, AC-7.5 | analyze | task1 |
| task25 | Global confirm-on-write queue arbiter (single-modal-at-a-time, queue UI with pending-approval indicator per tab, operation ID idempotency, `Rejected`/`Approved{operation_id}` reply protocol) | AC-7.1, AC-7.2, AC-7.3 | coding | task24, task4 |
| task26 | Terminal stdin confirm-on-write wiring (Terminal Mesh side: pause stdin write until approval without blocking output/resize/exit observation; cancel returns to claude as `Rejected`) | AC-7.4 | coding | task25, task15 |
| task27 | Write `docs/specs/gmail-sync.md` (OAuth loopback flow + browser-launch failure handling, historyId/historyNotFound recovery, 429 backoff curve with 5-min ceiling, account_id keying, initial-full-sync boundaries, Stronghold interrupted-setup recovery) | AC-5.1, AC-5.2, AC-5.3, AC-5.4 | analyze | task6 |
| task28 | Gmail sidecar scaffold: MCP server skeleton + OAuth loopback flow + browser-launch failure UX + duplicate-account detection | AC-5.1, AC-2.1 | coding | task27, task11 |
| task29 | Multi-account Stronghold storage (`plugin_id × account_id` keying) + Stronghold interrupted-setup detection and recovery flow + cleanup of orphan partial-OAuth SQLite state at startup | AC-5.2, AC-5.4 | coding | task28, task6 |
| task30 | Gmail incremental sync engine (historyId, historyNotFound controlled resync, 429 exponential backoff with 5-min ceiling and visible "syncing slower") | AC-5.3 | coding | task29, task5 |
| task31 | Gmail send via confirm-on-write with idempotent operation IDs (sidecar retries do not duplicate) | AC-5.5, AC-7.3 | coding | task30, task25 |
| task32 | Gmail modify operations (label, archive, mark-read, mark-unread, trash) gated by confirm-on-write with operation IDs; verify `gmail.delete` tool is NOT registered in MCP tool list | AC-5.6, AC-7.1 | coding | task30, task25 |
| task33 | Gmail inbox UI (Chinese strings) + account switcher + attachment list/open-on-demand with 60s temp-file cleanup; explicit NO compose/send UI buttons (Phase 2 deferral honored) | AC-5.1, AC-5.7 | coding | task31, task32 |
| task34 | Papers sidecar scaffold: MCP server skeleton | AC-2.1 | coding | task11 |
| task35 | Zotero three-mode access client (Live HTTP probe at `localhost:23119` → SQLite-ro fallback with `?mode=ro&immutable=1` → app-owned cached snapshot; default macOS path with user override in plugin settings; visible mode status indicator) | AC-6.1 | coding | task34, task5 |
| task36 | arxiv-rs query engine + DOI/arXiv-ID dedup + rule-based recommendations + polite caching guard (~30 min DIRECTIONAL) | AC-6.2, AC-6.3, AC-6.4 | coding | task35 |
| task37 | Papers tab UI (Chinese strings) with merged list + dedup badges + offline indicator | AC-6.1, AC-6.2 | coding | task36 |
| task38 | AppQuitCoordinator (write-acceptance freeze → cancel/resolve pending modals → SIGTERM PTYs +grace+SIGKILL → graceful sidecar Shutdown → force-kill after timeout; startup stale-process reaping for force-quit recovery) | AC-9.4 | coding | task11, task15, task25 |

## Claude-Codex Deliberation

### Agreements (post-round-1)
- Locked Decisions in v2 draft are absolute constraints; codex Phase 3 explicitly confirmed `QUESTIONS_FOR_USER = None`.
- `build.rs` codegen pipeline needs clean-on-build semantics + gitignored generated dirs.
- MCP stdio sidecars MUST log to stderr/files only; lint rule + integration test catch leakage.
- Per-claude-tab MCP config file approach for `claude` wiring.
- `claude` PATH discovery probes known paths + login-shell PATH; persisted; orchestrator onboarding card if missing.
- Confirm-on-write needs a global queue arbiter, not stacked modals.
- Terminal stdin confirmation pauses write but not output/resize/exit observation.
- Scope honesty: confirm-on-write covers plugin-mediated writes only.
- Stronghold interrupted-setup recovery is required.
- SQLite multi-process WAL access needs `busy_timeout` + advisory-lock-guarded migrations to coordinate SIBLING sidecar copies of the same plugin (not different plugins — Codex round-1 catch).
- Normal-quit sequencing leaves no orphans; force-quit can't guarantee cleanup hooks; startup stale-process reaping compensates.
- Structured JSON logs with correlation IDs + redaction.
- Logging is foundational and folded into task4 (dispatcher), not deferred to a late task.
- Per-command permission metadata is single-sourced from Rust annotations; both dispatcher and TS wrappers consume the same metadata.
- Manifest uniqueness checks (duplicate `plugin_id`, `db_namespace`, `command_bin`) enforced at build time.
- Gmail modify operations are an explicit MVP requirement (task32 added) — distinct from send (task31).
- Gmail tab UI MUST NOT include compose/send buttons in MVP (Phase-2 deferral honored explicitly in task33).
- `gmail.delete` tool MUST NOT be registered (`AC-5.6` negative test).
- Notification permission requested lazily on first event, not at startup.
- Zotero SQLite path has a default + user override in plugin settings.
- 429 backoff has an explicit 5-minute ceiling (not just "exponential backoff").
- Attachment temp files cleaned within 60 seconds of opening.

### Resolved Disagreements (round 1 → r2)
- **PluginCapability transport** (round 1): replaced "Arc<PluginCapability> in React closure" (impossible across Tauri IPC) with "branded opaque handle / unguessable nonce held in React closure; authoritative state in Rust MountRegistry by `(plugin_id, mount_id)`; validation at every IPC call".
- **Generated dirs wording** (round 1): "checked-in (gitignored)" → "gitignored and regenerated during build". No internal contradiction.
- **AC-1.3 contention test** (round 1): same-plugin sibling sidecars under N×M, not different plugins, is the real concurrent-access scenario.
- **AC-3.4 testing** (round 1): replaced memory-scanning with deterministic code-path assertion (orchestrator has no `secrets.gmail` permission scope; Stronghold gmail entries are inaccessible to orchestrator). Memory scan kept as optional diagnostic.
- **AC-9.4 force-quit** (round 1): claim only that NORMAL quit guarantees cleanup; force-quit/SIGKILL acknowledged as unable; startup stale-process reaping is the recovery mechanism.
- **Gmail modify ops missing** (round 1): added task32 covering label/archive/mark-read/mark-unread/trash via confirm-on-write.
- **No Gmail compose/send UI in MVP** (round 1): explicitly enforced in task33 description and Allowed Choices.
- **Logging task placement** (round 1): folded into task4 dispatcher (correlation IDs are foundational, not late-stage polish).
- **task6 traceability** (round 1): fixed `AC-4.3, AC-5.4` → `AC-5.2, AC-5.4`.
- **task7 traceability** (round 1): fixed `AC-1.5, AC-6.1` → `AC-1.5, AC-9.3`.

### Implementation-Decision Items (NOT user decisions — coding-phase choices)
- MCP Rust SDK: `rmcp` vs hand-written stdio wrapper.
- Type-codegen tool: `ts-rs` vs `specta`.
- Migration runner: `refinery` vs `sqlx::migrate!`.
- React state management: Zustand vs Context+reducer.
- Confirm-on-write queue scope: global (chosen at Lower Bound) vs per-plugin (alternative explicitly rejected for MVP simplicity).
- Generated source dir: gitignored + build-regenerated (chosen) vs committed.
- SQLite access pattern: pool per sidecar vs actor-owned connection (decide per-plugin during implementation).

### Convergence Status
- Round: 2 (after applying round-1 required changes). Awaiting round-2 verification.

## Pending User Decisions

- None. Codex Phase 3 and Phase 5 round 1 both returned no user-facing decisions. All architectural decisions are pre-locked via the 6 rounds of user clarification captured in the v2 draft's "Locked Decisions" section.

## Implementation Notes

### Code Style Requirements
- Implementation code and comments must NOT contain plan-specific terminology such as "AC-", "Milestone", "Step", "Phase", or similar workflow markers.
- These terms are for plan documentation only, not for the resulting codebase.
- Use descriptive, domain-appropriate naming in code instead.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_01-26-52
- Tool: codex
