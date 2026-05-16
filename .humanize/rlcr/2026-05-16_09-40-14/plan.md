# MCP-Federated Personal Agent Platform With Privileged Orchestrator And Per-Tab Workspaces — MVP

## Goal Description

Implement the MVP of a Tauri v2 desktop application whose three plugin tabs (Terminal Mesh, Gmail multi-account, Papers via Zotero+arXiv) run as OS-process-isolated MCP-server sidecars, plus a privileged single-instance orchestrator tab that auto-launches `claude` with full platform context. Each Terminal Mesh tab is bound to one workspace (real directory under `~/AgentPlatform/workspaces/<name>/`, user-visible and IDE-accessible). Long-running terminal/agent state surfaces through a host-owned dedup-aware notification surface combining native macOS notifications and a menubar tray window. All locked decisions in the v2 draft are constraints, not options.

## Acceptance Criteria

Following TDD: each criterion includes positive (must-pass) + negative (must-fail) tests. Quantitative thresholds marked HARD (user-confirmed must-meet) or DIRECTIONAL (same-order-of-magnitude acceptable).

- **AC-1: Plugin Contract Pipeline is declarative and host-agnostic.** Adding a plugin requires only creating `plugins/<id>/` with `plugin.toml` + Rust adapter crate + frontend dir. The host's `build.rs` discovers plugins, emits Rust registry + frontend tab registry (`src/generated/plugin-tabs.ts`) + per-plugin TS wrappers — without edits to host source, router enums, command match arms, or tab switch statements.
  - AC-1.1: `plugin.toml` schema enforced. Required fields: `name`, `version`, `type` (`tab` for MVP), `command_bin` (sidecar binary name), `frontend` (relative path to frontend entry), `permissions[]`, `required_apis[]`, `db_namespace`, `migrations_path`. Manifest uniqueness checks: duplicate `plugin_id`, duplicate `db_namespace`, or duplicate `command_bin` across plugins fails the build.
    - Positive Tests: a valid manifest registers the plugin and surfaces its tab at app launch with declared permission set visible in the registry.
    - Negative Tests: a manifest missing any required field fails the build (`build.rs` aborts with typed error naming the field); a manifest declaring an unknown `required_apis` value fails the build; two plugins declaring the same `db_namespace` fail the build with a typed error naming both plugin IDs.
  - AC-1.2: Typed IPC code-gen pipeline emits BOTH Rust registry AND frontend TS wrappers. Generated outputs land at known repo-local paths that are gitignored and regenerated during each build. Clean-on-build avoids stale artifacts.
    - Positive Tests: TS calls go through `plugins.<id>.commands.<commandName>(args, capability)` style generated functions; `tsc` catches wrong arg types at compile time. Removing a plugin and rebuilding produces a TS compile error for any code still calling its removed commands. Per-command permission metadata is generated alongside (single source of truth: same Rust `#[command(permissions=["user.email"])]` annotation drives both the dispatcher's gate and the TS wrapper's docstring).
    - Negative Tests: raw `invoke('plugin.<id>.<command>', args)` calls are not used anywhere in plugin frontend code (enforced by lint rule or convention check).
  - AC-1.3: Each plugin owns its own SQLite database file at `${APP_DATA}/plugins/<id>/state.sqlite` with WAL journaling, `busy_timeout` configured (default 5000ms), advisory-lock-guarded migrations to prevent concurrent migration runs from sibling sidecar copies.
    - Positive Tests: under the N×M lifecycle, multiple sibling sidecar copies of the SAME plugin (e.g., 3 concurrent Gmail sidecars from 3 claude tabs) all opening `${APP_DATA}/plugins/gmail/state.sqlite` complete a 100-operation read/write stress test without races or avoidable `database is locked` failures; only ONE sidecar's migration runs to completion on startup (advisory lock), others wait then resume against the migrated schema.
    - Negative Tests: the host exposes no API allowing plugin A to open or query plugin B's `state.sqlite` file; cross-plugin SQL is impossible.
  - AC-1.4: Permission gating enforced at the Rust IPC dispatcher (mandatory; warning-only is forbidden). The dispatcher rejects any call whose target command requires a permission the calling plugin's manifest does not declare. Per-command permission metadata is single-sourced from Rust annotations.
    - Positive Tests: a plugin manifest with `permissions: ["user.email"]` can invoke Gmail-scoped commands; the dispatcher logs (caller plugin_id, target command, permission match) with correlation IDs.
    - Negative Tests: a plugin without `user.email` calling a Gmail-scoped command receives a typed `PermissionDenied { caller_plugin_id, missing_permission, requested_command }` error before the target ever executes.
  - AC-1.5: Caller identity via `PluginCapability` is a **branded opaque handle** (unguessable server-issued nonce or signed token), held only in the plugin component's React closure (NOT on `window`, NOT in `localStorage`/`sessionStorage`/`IndexedDB`, NOT serializable to user-readable JSON). Authoritative capability state lives in Rust's `MountRegistry` keyed by `(plugin_id, mount_id)`. The handle is required on every permissioned IPC call; the dispatcher validates the handle against the registry on every call.
    - Positive Tests: a permissioned wrapper invoked from plugin A's component (carrying A's handle) succeeds. Calling the same wrapper from plugin B's component (carrying B's handle, distinct nonce) fails with `PermissionDenied` because the Rust dispatcher looks up `(plugin_id from handle, mount_id from handle)` and rejects mismatches.
    - Negative Tests: calls carrying a forged handle (random bytes), an expired handle (post-unmount, registry entry removed), a handle bound to a different `(plugin_id, mount_id)` pair, or no handle at all are rejected with typed errors (`CapabilityInvalid`, `CapabilityExpired`, `CapabilityMismatched`, `CapabilityMissing`). In-flight IPC calls during a remount complete atomically against the old handle OR get rejected with `CapabilityExpired` (no half-states with mixed pre/post-rotation state).
  - AC-1.6: Plugin sidecar lifecycle: host-owned UI sidecars vs per-claude-tab sidecars have distinct identities (separate `client_id`s like `host_ui:<plugin_id>` vs `claude:<tab_id>:<plugin_id>`; separate log prefixes; separate capability scopes). Each sidecar handles a `Shutdown` IPC gracefully and is force-killed after a 5-second timeout.
    - Positive Tests: spawning a per-claude sidecar from a fresh `claude` tab creates a sidecar with a unique `client_id` visible in logs; sending `Shutdown` results in clean exit within 5s.
    - Negative Tests: a sidecar that ignores `Shutdown` is SIGKILL'd at 5s; orphaned sidecars do not persist in `ps aux` after host process exit.
  - AC-1.7: Sidecar crash triggers auto-restart with exponential backoff (1s → 2s → 4s → … → 60s upper cap; resets after 5 minutes of stable uptime). The affected plugin's tab shows an error state during restart with a manual retry button. Other plugins are unaffected.
    - Positive Tests: `kill -9 <gmail sidecar pid>` triggers restart attempts logged with correlation IDs at +1s, +2s, +4s; Papers tab continues to render normally throughout.
    - Negative Tests: the Gmail tab does not silently disappear or render a white screen; orchestrator's `claude` sees the disconnected MCP server and surfaces an error in that tool call but does not crash.

- **AC-2: MCP Stdio Server Discipline + Claude Wiring.**
  - AC-2.1: Every plugin sidecar exposes an MCP server over stdio with correct framing. Logs are written ONLY to stderr or a per-process log file at `${APP_DATA}/logs/<plugin_id>/<client_id>.log`. Stdout carries protocol bytes only.
    - Positive Tests: piping a sidecar's stdout into a strict MCP parser during a 60-second log-heavy test run produces zero framing/deserialization errors.
    - Negative Tests: introducing a `println!`/`console.log` into sidecar code during dev causes test failure (lint rule + integration test catch it).
  - AC-2.2: Per-claude-tab MCP config generator. When a tab launches `claude`, the host writes an ephemeral MCP config file (e.g. `${APP_DATA}/claude-mcp-configs/<tab_id>.json`) listing exactly one sidecar invocation per plugin for that tab, then passes its path to `claude` via the documented CLI mechanism. Tab/workspace identity is passed via sidecar args or env vars.
    - Positive Tests: opening a new Terminal Mesh tab and running `claude` results in `claude` having access to all MVP plugin MCP servers; sidecar logs show the spawning `tab_id` and `workspace_path`.
    - Negative Tests: closing the tab removes the config file; orphan config files older than the host's launch timestamp are cleaned at next startup.
  - AC-2.3: `claude` CLI PATH discovery does NOT rely on GUI app shell inheritance. On first launch, the host probes (a) known install paths (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`), (b) the user's login-shell PATH via `bash -lc 'command -v claude'`. The discovered path is persisted in app config.
    - Positive Tests: launching the app via Finder (no inherited terminal PATH) finds `/opt/homebrew/bin/claude` on Apple Silicon Macs.
    - Negative Tests: if `claude` is absent in all probed paths, the orchestrator shows a Chinese onboarding card listing searched paths + a "选择 claude 路径" file picker — and does NOT spawn empty sidecar processes.

- **AC-3: Orchestrator is a privileged single-instance host tab.**
  - AC-3.1: Top-left fixed tab position with distinct icon. The host enforces single-instance semantics: opening "Orchestrator" while one already exists focuses the existing tab.
    - Positive Tests: clicking the orchestrator slot twice never spawns two tabs.
    - Negative Tests: closing the orchestrator and reopening it spawns a fresh tab with rotated capabilities.
  - AC-3.2: On open, the orchestrator's PTY auto-launches `claude` with the platform's MCP config preconfigured (one sidecar per MVP plugin); if `claude` is unavailable, it shows the onboarding card (per AC-2.3).
    - Positive Tests: opening the orchestrator from a cold start shows the `claude` prompt with MCP servers connected (verifiable via `claude` tool listing).
    - Negative Tests: opening with `claude` absent does NOT fork-exec any plugin sidecars.
  - AC-3.3: Cross-tab read capability for the orchestrator. The orchestrator's `PluginCapability` for the Terminal Mesh MCP server includes a `cross_tab_read=true` permission flag; this flag is granted ONLY by the Rust dispatcher and is never present in any object visible to frontend serialization (it lives in the MountRegistry alongside the orchestrator's capability handle).
    - Positive Tests: orchestrator's claude can call `terminal_mesh.read_scrollback(tab_id=X)` for any tab and receive a bounded response (max-bytes documented per call).
    - Negative Tests: a regular tab's claude calling the same tool for a different `tab_id` receives `PermissionDenied`. Serializing any object reachable from the frontend (window, React state, capability handle string itself) reveals NO `cross_tab_read` flag — the flag exists only in Rust state.
  - AC-3.4: Gmail send via orchestrator's OAuth delegation. The orchestrator's claude has NO Gmail credentials of its own; it requests `gmail.send` via the Gmail sidecar's MCP tool, which (after host confirm-on-write approval) reads the token from Stronghold and dispatches the send.
    - Positive Tests: orchestrator can complete a Gmail send via claude → MCP call → confirm-on-write modal → user approves → Gmail sidecar uses Stronghold-stored token → API call. Token is at NO point passed through orchestrator's process boundary, claude's stdin/stdout, MCP config file, environment variables, or logs (verified by audit of all these surfaces).
    - Negative Tests: code-path assertion — orchestrator declares no `secrets.gmail` permission in its capability scope; the orchestrator has no path to Stronghold's Gmail entries. (Memory scanning is offered as an OPTIONAL diagnostic, NOT a required AC test.)
  - AC-3.5: Orchestrator's claude task-complete events trigger a notification with a semantic summary (e.g., "claude: 标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐") rather than "exit 0".
    - Positive Tests: a long claude task completing emits one tray entry + one native notification containing claude's `task_summary` (provided via OSC sequence or MCP notification).
    - Negative Tests: a raw `bash` exit in the orchestrator triggers the regular completion notification, NOT the semantic-summary path.

- **AC-4: Terminal Mesh + Workspaces.**
  - AC-4.1: At least **4 concurrent PTY sessions** can be open simultaneously [HARD, user-confirmed]. Each runs a Tokio actor with a **1 MB bounded ring buffer per PTY** [HARD, user-confirmed], handling UTF-8 multi-byte and ANSI escape sequence boundaries safely.
    - Positive Tests: spawn 4 PTYs running a portable synthetic fast-output command (e.g. `yes | head -c 100M` or `seq 1 1000000`); all four render concurrently in xterm.js without visible blocking or garbled output. Spawning a 5th PTY succeeds.
    - Negative Tests: closing a PTY tab while a command runs terminates the child process cleanly (verified via `ps aux` showing no orphan with the workspace's cwd); a 10MB burst into a single PTY's stdout does not OOM the host (ring overflow coalesces oldest bytes per documented policy).
  - AC-4.2: PTY supports stdin / SIGWINCH resize / exit-status reporting / cwd / env at spawn.
    - Positive Tests: typing reaches the child; viewport resize propagates SIGWINCH; child exit code surfaces in the tab's status bar; a PTY spawned with `cwd=~/AgentPlatform/workspaces/foo` reports that cwd via `pwd`.
    - Negative Tests: spawning with an invalid shell path returns a typed error and renders an in-tab error state without crashing the app.
  - AC-4.3: Each Terminal Mesh tab is bound to exactly ONE workspace. Workspaces live at `~/AgentPlatform/workspaces/<name>/` (user-visible, accessible by Cursor/VS Code) OR point to a user-selected existing directory.
    - Positive Tests: creating a new workspace named "debug-arxiv" creates `~/AgentPlatform/workspaces/debug-arxiv/`; picking an existing directory associates the tab with it without copying or modifying it.
    - Negative Tests: workspace names containing forbidden filesystem characters are rejected at the modal level with a typed error; the platform never auto-creates `.git/` in the workspace dir.
  - AC-4.4: Workspaces are persistent across app restart. One workspace = AT MOST one tab at a time, enforced by canonical-path resolution (symlinks resolved, case-insensitive paths normalized on macOS).
    - Positive Tests: closing the app and reopening it shows previously-created workspaces in the switcher; reopening a workspace restores its tab and its `.claude/` conversation context.
    - Negative Tests: clicking a workspace already open in another tab focuses that tab (no duplicate); creating two workspaces whose user-selected dirs resolve to the same canonical path is rejected at creation with a typed error.
  - AC-4.5: Workspace switcher modal triggered by the tab strip's `+` button. Two affordances: "Create new" (name input + path source: auto OR pick existing dir) and "Pick recent" (scrollable list). Each list row shows: name + last-used timestamp + created timestamp + claude conversation rounds count (extracted from `.claude/`).
    - Positive Tests: clicking `+` opens the modal; typing a name + clicking Create produces a new tab with the named workspace.
    - Negative Tests: the modal is not stacked on top of a confirm-on-write modal (modal coordinator enforces single-active-modal); cancelling the modal closes it cleanly without creating any artifacts.
  - AC-4.6: Per-workspace IDE handoff. Each workspace row (in switcher AND in active tab's settings) exposes "Open in Cursor" (configurable to VS Code / Zed / other) and "Reveal in Finder".
    - Positive Tests: clicking "Open in Cursor" invokes `cursor <workspace_path>` (or configured alternative).
    - Negative Tests: if the configured IDE command is not in PATH, the button shows a typed error toast instead of failing silently.

- **AC-5: Gmail Plugin (multi-account, OAuth, incremental sync, modify operations).**
  - AC-5.1: At least **2 Gmail accounts** added independently [HARD, user-confirmed]. OAuth via system browser loopback. Per-account state isolation in Stronghold and SQLite.
    - Positive Tests: adding two accounts produces two switchable identities; each shows its own inbox; revoking account A externally produces a typed re-auth prompt for A without affecting B.
    - Negative Tests: browser-launch failure during OAuth produces a typed error with a "重试" option; duplicate-account-add detection rejects re-adding the same `account_id` with a typed error.
  - AC-5.2: OAuth refresh tokens stored ONLY in `tauri-plugin-stronghold`, keyed by `(plugin_id, account_id)`. Access tokens stay in process memory.
    - Positive Tests: scanning all plugin SQLite files post-OAuth finds zero refresh-token strings; scanning WebView storage finds zero tokens.
    - Negative Tests: removing an account triggers Stronghold key deletion + SQLite metadata cleanup; no token survives account removal.
  - AC-5.3: Incremental sync via Gmail `historyId`. `historyNotFound` triggers controlled resync with visible progress. `429 Too Many Requests` triggers exponential backoff with documented ceiling: cap of 5 minutes between retries; reset on successful response.
    - Positive Tests: app restart resumes from last-stored `historyId`; only new messages fetched. A `429` response triggers exponential backoff visible as "syncing slower"; ceiling at 5min between retries.
    - Negative Tests: an invalid `historyId` (`historyNotFound`) triggers controlled resync, not a crash; backoff never effectively stalls (5min ceiling).
  - AC-5.4: Stronghold setup recovery. Interrupted master-password setup can be resumed or reset cleanly without leaving partially-authorized accounts.
    - Positive Tests: killing the app during initial Stronghold setup, then relaunching, presents either "继续设置" or "重置 vault" with explanation of consequences.
    - Negative Tests: no plaintext token appears in any file after an interrupted setup; orphaned partial-OAuth state in SQLite is detected and cleaned at next startup.
  - AC-5.5: Send via Gmail plugin requires confirm-on-write approval. Operation IDs ensure idempotency on sidecar retries.
    - Positive Tests: a `gmail.send` call from any claude tab pops a confirm modal showing recipient + subject + body preview + Confirm/Cancel; confirming dispatches exactly one send; sidecar retry with the same operation ID does NOT cause a duplicate send.
    - Negative Tests: cancelling the confirm returns a typed `Rejected` error to claude; no Gmail API call is made.
  - AC-5.6: Modify operations (label / archive / mark-read / mark-unread / trash) gated by confirm-on-write. No permanent delete (`gmail.delete` scope NOT requested in MVP).
    - Positive Tests: a `gmail.label`, `gmail.archive`, `gmail.mark_read`, `gmail.mark_unread`, or `gmail.trash` call from any claude tab pops a confirm modal with action description + target message metadata + Confirm/Cancel; confirming executes the modify; cancelling returns `Rejected`.
    - Negative Tests: `gmail.delete` (permanent delete) is NOT in the MVP MCP tool list — code lookup confirms no such tool is registered.
  - AC-5.7: Attachments listed inline, opened on-demand. No auto-download, no indexing. Attachment temp files cleaned up within 60 seconds of opening or at app exit (whichever is sooner).
    - Positive Tests: clicking an attachment opens it via macOS default app (`open` shell-out or `NSWorkspace`).
    - Negative Tests: temp files from attachment opens are absent from disk within 60s; closing the inbox tab does not leave temp files lingering past the 60s window. Gmail tab UI does NOT include compose/send buttons (those are Phase 2; send happens only via claude in MVP).

- **AC-6: Papers Plugin (Zotero three-mode + arXiv).**
  - AC-6.1: Zotero access in priority order: (1) **Live** — HTTP at `localhost:23119` when Zotero is running and local API is enabled; (2) **SQLite read-only fallback** — `?mode=ro&immutable=1` on `zotero.sqlite` when Zotero is closed but the file is readable; (3) **App-owned cached snapshot** — the plugin's own materialized cache when both above fail. Each mode has a visible status indicator. Zotero SQLite path discovery: default macOS path (`~/Zotero/zotero.sqlite`); user can override in plugin settings.
    - Positive Tests: with Zotero running, Live path is taken; closing Zotero falls back to SQLite-ro within one health-check cycle; deleting/locking `zotero.sqlite` falls back to cached snapshot with a "offline — using snapshot from <timestamp>" banner; user-configured Zotero path is honored.
    - Negative Tests: the plugin never opens `zotero.sqlite` in read-write mode; a corrupted Zotero SQLite file (catch deserialization error) produces cached-snapshot fallback rather than crashing.
  - AC-6.2: arXiv via `arxiv-rs`; results merged with Zotero items and deduplicated by DOI / arXiv ID.
    - Positive Tests: an arXiv item already in Zotero by DOI appears once with an "in-library" badge.
    - Negative Tests: items with same author/title prefix but distinct DOIs are NOT collapsed.
  - AC-6.3: Rule-based recommendations only (categories + Zotero tag affinity). No personalized ranking.
    - Positive Tests: subscribing to `cs.LG` produces a fresh list filtered by current arXiv submissions.
    - Negative Tests: removing a subscription stops new items appearing; no implicit "remember-my-clicks" behavior.
  - AC-6.4: Polite caching: target **~30 minute minimum refresh interval per query** [DIRECTIONAL — same-order-of-magnitude acceptable]. Manual refresh respects a short rate guard.
    - Positive Tests: clicking refresh twice rapidly produces only one network call.
    - Negative Tests: programmatic poll rate per query does not exceed configured minimum by more than one order of magnitude.

- **AC-7: Confirm-on-Write Surface.**
  - AC-7.1: Every plugin-mediated write originating from a claude tab triggers a host-owned confirm modal showing action description + payload preview + Confirm/Cancel.
    - Positive Tests: a `gmail.label` call shows a modal with "添加标签 'Important' 到邮件 X"; confirming labels the email; cancelling returns `Rejected`.
    - Negative Tests: a read-only call (e.g., `gmail.list_inbox`) does NOT trigger a confirm modal.
  - AC-7.2: Concurrent write requests across tabs are arbitrated by a host-owned global queue, not stacked modals.
    - Positive Tests: two simultaneous `gmail.label` calls from two tabs are serialized; the second modal appears after the first is dismissed; queued requests show a "pending approval" indicator in their originating tab.
    - Negative Tests: there is never more than one open confirm modal at a time across the app.
  - AC-7.3: Approval carries an idempotent operation ID. A sidecar retrying after approval (e.g., transient network error) reuses the same operation ID to avoid duplicate writes.
    - Positive Tests: a sidecar retry with the same operation ID against an already-completed approval returns the cached result, not a fresh write.
    - Negative Tests: a retry with a different operation ID is treated as a new request requiring fresh approval.
  - AC-7.4: Terminal stdin confirmation (claude writing into another tab's PTY via orchestrator) pauses the write but does NOT block the destination PTY's output stream, resize events, or exit observation.
    - Positive Tests: while a stdin confirm modal is open, the destination PTY's output continues rendering; the PTY can be resized; child exit is observed and surfaced.
    - Negative Tests: cancelling the stdin confirm does not corrupt the destination PTY's state.
  - AC-7.5: Scope honesty. Confirm-on-write covers plugin-mediated writes (Gmail send/label/etc., Papers add/edit, Terminal stdin from orchestrator). Raw filesystem writes by claude inside a workspace's own dir are NOT intercepted in MVP. This boundary is documented in the orchestrator onboarding card and project README.
    - Positive Tests: a `gmail.send` IS gated.
    - Negative Tests: claude writing `notes.md` in its own workspace via `Edit`/`Write` tool is NOT gated (and is not claimed to be in any docs or UI).

- **AC-8: Notification Surface ("灵动岛" analog).**
  - AC-8.1: Native macOS notification + menubar tray window (`TrayBottomCenter`) both fire on terminal completion. Notification permission requested on first event that would trigger a notification (lazy, not at app startup).
    - Positive Tests: `sleep 10 && echo done` produces one banner + one tray entry; first such event triggers the permission prompt.
    - Negative Tests: a tight loop of fast-completing commands produces at most one notification per `(plugin_id, terminal_id, event_kind)` per ~2s dedup window.
  - AC-8.2: Default needs-attention triggers: child exit 0, nonzero exit, OSC agent marker, prompt-waiting heuristic (where reliably detectable). Stderr-burst is NOT a default trigger.
    - Positive Tests: OSC agent marker triggers a needs-attention tray entry.
    - Negative Tests: writing a large blob to stderr does NOT trigger an alert.
  - AC-8.3: Orchestrator's claude task-complete fires a notification with a claude-generated semantic summary.
    - Positive Tests: a multi-step claude task surfaces a summary like "标记 3 封邮件为已读".
    - Negative Tests: a non-claude command in the orchestrator triggers the regular completion event, not the semantic-summary path.
  - AC-8.4: Notification permission denial is graceful.
    - Positive Tests: with notifications denied, an in-app banner explains the fallback; tray window continues to drive event surfacing.
    - Negative Tests: denied permission does NOT cause a runtime panic or repeated re-prompts.

- **AC-9: Application Shell Robustness + First-Run Bootstrap + Quit Sequencing.**
  - AC-9.1: First-run bootstrap creates required dirs: `~/AgentPlatform/`, `~/AgentPlatform/workspaces/`, `${APP_DATA}/plugins/<id>/`, `${APP_DATA}/logs/`, `${APP_DATA}/claude-mcp-configs/`. Permission failures surface a user-readable error.
    - Positive Tests: first launch on a fresh user account creates all required dirs and shows the empty workspace switcher.
    - Negative Tests: a permission failure (e.g., read-only home dir) shows a Chinese error card with the offending path and remediation hint instead of crashing.
  - AC-9.2: Cold start with no plugin sidecars built/bundled shows a dev-diagnostics page listing missing binaries and a hint to run the build command.
    - Positive Tests: in a dev checkout where the build has not produced sidecar binaries, the app launches and shows "缺失 plugin 二进制：…" with the list of expected binaries.
    - Negative Tests: production builds with all sidecars bundled never show this page.
  - AC-9.3: SQLite migration failure for one plugin blocks that plugin's mount, shows an error state in its tab, and leaves other plugins unaffected.
    - Positive Tests: a simulated Gmail migration error keeps Papers + Terminal Mesh running; the Gmail tab shows error state with a "重试迁移" button.
    - Negative Tests: migration failure does NOT crash the host process.
  - AC-9.4: App quit sequence: stop accepting new writes → resolve/cancel pending confirm modals → terminate PTYs (SIGTERM + grace + SIGKILL) → send graceful Shutdown to all sidecars → force-kill after timeout. No orphaned sidecars or PTY children remain after **normal quit**. **OS force-quit / `kill -9` of the host is acknowledged as unable to guarantee cleanup hooks**; the host MUST detect and reap stale app-owned process records at next startup where possible.
    - Positive Tests: `lsof` and `ps aux` after a normal quit show no orphan processes attributable to the app. On next launch after a force-quit, startup cleanup scans `${APP_DATA}/sidecar-state/` for stale PID records and attempts to reap them.
    - Negative Tests: normal quit (Cmd+Q / app menu Quit) never leaves orphans. The plan and onboarding docs do NOT claim cleanup hooks fire on force-quit / SIGKILL.
  - AC-9.5: Structured JSON logs per process with correlation IDs (`tab_id`, `plugin_id`, `request_id`). OAuth tokens and email bodies are redacted in logs.
    - Positive Tests: a request/event log entry carries correlation IDs joinable across host + sidecar logs.
    - Negative Tests: grepping logs for token-shaped strings (long base64-ish near `refresh_token` keys) or for email-body content returns zero matches.

## Path Boundaries

> Lower Bound = minimum implementation that still passes ALL ACs. Upper Bound = polished MVP. Slack lives in UI polish, error-message richness, observability depth — NOT in AC-required behaviors.

### Upper Bound (Maximum Acceptable Scope)
- Plugin contract: full manifest schema with optional metadata (icons, sort order); polished error UI per plugin; lint rule banning raw `invoke('plugin.…')`; `cargo make`-style build orchestration; manifest uniqueness checks.
- MCP wiring: per-tab MCP config files with realtime cleanup AND startup GC; sidecar log files rotated by size; structured JSON dev logs with correlation IDs visible in dev tools.
- Orchestrator: polished Chinese onboarding card with `claude` path picker; semantic-summary notification with action-count breakdown; cross-tab read bounded byte cap visible in settings.
- Terminal Mesh: ≥4 PTYs (HARD), 1 MB ring (HARD), UTF-8/ANSI-safe boundary handling, polished tab-close animations, drag-to-reorder, switcher modal with debounced search.
- Gmail: multi-account read + modify (label/archive/mark-read/mark-unread/trash) + send (via claude only). Polished compose preview in confirm-on-write modal. Attachment list/open-on-demand. Polished re-auth UX.
- Papers: full three-mode Zotero with visible mode status; arXiv polite caching with per-query refresh times shown; in-library badge with hover tooltip.
- Confirm-on-write: global queue with pending-approval count badge in tab strip; idempotent operation IDs surfaced in approval modal for debug.
- Notifications: native + tray + semantic summaries; permission-denial fallback banner; dedup arbiter inspectable in dev tools.
- Application shell: structured JSON logs with redaction; quit sequencing with progress indicator; Chinese UI strings throughout with `i18n` key layer ready for Phase 2.
- Packaging: macOS signed/notarized `.app` is STRETCH.

### Lower Bound (Minimum Acceptable Scope — still passes ALL ACs)
- Plugin contract: manifest enforced (+ uniqueness checks), codegen pipeline with gitignored generated dirs, per-plugin DB files with WAL + busy_timeout + advisory-lock migrations, mandatory permission gating, branded-handle `PluginCapability` validated against MountRegistry, distinct host-UI vs per-claude sidecar identities, auto-restart with exponential backoff (all AC-1 sub-criteria required).
- MCP wiring: stdio framing with logs to stderr/files; per-tab MCP config generator (orphan cleanup at next startup at minimum); `claude` PATH discovery probes known paths + login-shell PATH.
- Orchestrator: top-left fixed single-instance auto-launches claude; cross-tab read works for orchestrator only with `cross_tab_read` flag in Rust state; Gmail send via OAuth delegation; semantic-summary notification.
- Terminal Mesh: ≥4 concurrent PTYs HARD; 1 MB ring HARD; UTF-8/ANSI-safe slicing; workspace creation + persistence + canonical-path uniqueness; switcher modal; Open in Cursor + Reveal in Finder.
- Gmail: ≥2 accounts HARD; OAuth via system browser; Stronghold-only tokens; historyId incremental sync with 5-min backoff ceiling; Stronghold interrupted-setup recovery; send + modify ops (label/archive/mark-read/unread/trash) via confirm-on-write with operation IDs; attachment list/open-on-demand; NO compose/send UI buttons.
- Papers: all three Zotero modes (Live/SQLite-ro/cached snapshot) with default + user-overridable path; DOI/arXiv-ID dedup; rule-based recommendations; polite caching (~30 min DIRECTIONAL).
- Confirm-on-write: every plugin-mediated write gated; global queue prevents stacked modals; operation IDs for idempotency; terminal stdin pause without output block; scope honesty documented.
- Notifications: native + tray on completion (with lazy first-event permission request); dedup keyed by `(plugin_id, terminal_id, event_kind)` ~2s; semantic-summary path for orchestrator's claude; permission-denial graceful fallback.
- Application shell: first-run bootstrap; dev-diagnostics page when no plugins built; plugin migration failure plugin-scoped; normal-quit sequencing with no orphans + startup-time stale-process reaping; structured logs with correlation IDs and redaction.
- Chinese UI strings.

### Allowed Choices
- **Can use**: Tauri v2; React 18+ / TypeScript; Rust (stable, 2024 edition); `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold`, `rusqlite` (WAL + busy_timeout); `google-gmail1` (Gmail REST); `arxiv-rs`; Zotero local HTTP API + `zotero.sqlite` read-only.
- **Implementation-decision items (NOT user decisions — deferred to coding phase)**: MCP Rust SDK (`rmcp` vs hand-written stdio); type-codegen (`ts-rs` vs `specta`); migration runner (`refinery` vs `sqlx::migrate!`); React state (Zustand vs Context+reducer); log shape (JSON via `tracing` vs custom); SQLite access pattern (pool per sidecar vs actor-owned connection).
- **Cannot use**: Electron; pure-web SPA; SwiftUI / AppKit-native; tmux-as-system-of-record; notebook-cell UI; in-process plugins; raw `invoke('plugin.…')` strings in plugin code; hand-maintained plugin enumeration; encrypted SQLite (Stronghold for secrets only); community-MCP-server forks (self-written sidecars only); macOS App Sandbox / per-plugin entitlements for MVP (OS process boundary only); send/reply UI buttons in Gmail tab (Phase 2); per-plugin sidecar dedup proxy; `gmail.delete` scope (Phase 2).

## Feasibility Hints and Suggestions

> **Note**: This section is conceptual reference, not prescriptive.

### Conceptual Approach

1. **Host skeleton** — Tauri v2 + React/TS shell. Tab strip with fixed orchestrator slot top-left and a `+` for workspace tabs. Backend exposes `PluginHost` trait + IPC dispatcher.
2. **Plugin contract** — `plugins/<id>/plugin.toml` + Rust adapter crate + frontend dir. `build.rs` discovers plugins; emits Rust registry, frontend tab registry, per-plugin TS wrappers into gitignored generated dirs (`src-tauri/src/generated/`, `src/generated/`); regenerated from clean each build.
3. **Typed IPC codegen** — `ts-rs` or `specta` (impl-decide). Rust command/event/error types are SoT. Per-command permission metadata generated from `#[command(permissions=["..."])]` annotations into BOTH dispatcher gating and TS wrapper docstrings.
4. **PluginCapability (branded handle)** — host issues a server-side nonce (e.g., random 128-bit token) on mount; passes through Tauri IPC to React `usePluginCapability` Context; React closure holds it; generated wrappers carry it as hidden first arg. Rust `MountRegistry` keyed by `(plugin_id, mount_id)` stores `{handle_nonce, permissions, cross_tab_read_flag}`. Validation on every IPC: dispatcher looks up registry, rejects forged/expired/mismatched/missing.
5. **MCP stdio sidecars** — pick `rmcp` or hand-written. Stdout protocol-only; logs to `${APP_DATA}/logs/<plugin_id>/<client_id>.log`. Lint/CI rule catches `println!`/`console.log` in sidecar code. `Shutdown` IPC → flush → exit; host waits 5s then SIGKILL.
6. **claude wiring** — host generates `${APP_DATA}/claude-mcp-configs/<tab_id>.json` listing per-plugin sidecar invocations with `--client-id claude:<tab_id>:<plugin_id>` + `--workspace <workspace_path>`. Tab launches `claude --mcp-config <path>`. Cleanup on close + startup GC.
7. **claude PATH discovery** — first-launch probe `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`; fall back to `bash -lc 'command -v claude'`; persist. Orchestrator onboarding card if missing.
8. **Per-plugin SQLite** — `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;`. Migration via `refinery` or `sqlx::migrate!`. Migration acquires advisory file lock (`state.sqlite.migration-lock`) so only one sibling sidecar runs migrations; others wait then resume against migrated schema.
9. **Stronghold** — refresh tokens at `(plugin_id, account_id)` keys. Master-password setup with resume/reset via marker file `${APP_DATA}/stronghold-state/setup.marker`.
10. **Terminal Mesh** — `portable-pty` Tokio actor with `VecDeque<u8>` 1 MB ring + drop-oldest-with-coalesce on overflow. UTF-8/ANSI boundary scanner before slicing. xterm.js subscribes via typed events.
11. **Workspace persistence** — central registry `${APP_DATA}/workspaces.json` (canonical paths, names, timestamps). Optional `.workspace.json` in workspace dir overrides metadata. Conversation rounds = parse `.claude/` files best-effort.
12. **Confirm-on-write queue** — host arbiter receives `RequestApproval { operation_id, action, payload }` from sidecars; queues; opens one modal at a time; replies `Approved {operation_id}` or `Rejected`. Sidecar holds `oneshot` awaiting reply.
13. **Notification surface** — host subscribes to plugin events with `notify` capability. Dedup keyed by `(plugin_id, terminal_id, event_kind)` over ~2s. Tray window `TrayBottomCenter` via positioner; native via tauri-plugin-notification. Permission request lazy on first event.
14. **App quit** — `AppQuitCoordinator`: stop write-acceptance → resolve modals → SIGTERM PTYs (5s grace, then SIGKILL) → graceful sidecar Shutdown → SIGKILL after timeout. Startup cleanup scans `${APP_DATA}/sidecar-state/` for stale records.

### Specification Deliverables (produced by `analyze`-tagged tasks)
- `docs/specs/plugin-contract.md`
- `docs/specs/mcp-sidecar.md`
- `docs/specs/claude-launch.md`
- `docs/specs/terminal-events.md`
- `docs/specs/confirm-on-write.md`
- `docs/specs/gmail-sync.md`

### Relevant References
- Tauri v2: https://v2.tauri.app/concept/architecture/, https://v2.tauri.app/develop/plugins/, https://v2.tauri.app/develop/sidecar/
- MCP spec (2025-11-25): https://modelcontextprotocol.io/specification/2025-11-25
- Claude Code MCP: https://code.claude.com/docs/en/mcp
- xterm.js, portable-pty, google-gmail1, arxiv-rs (cited in draft)
- Terax AI working precedent: https://github.com/crynta/terax-ai
- ts-rs: https://github.com/Aleph-Alpha/ts-rs ; specta: https://github.com/oscartbeaumont/specta

## Dependencies and Sequence

### Milestones

1. **Plugin Contract Foundation**
   - Phase A: Tauri scaffold + React/TS shell + fixed-orchestrator-slot tab strip.
   - Phase B: Write `docs/specs/plugin-contract.md`.
   - Phase C: `build.rs` codegen pipeline.
   - Phase D: Generated Rust IPC dispatcher with structured JSON logging + correlation IDs (logging is foundational and built alongside dispatcher).
   - Phase E: Per-plugin SQLite framework with WAL + busy_timeout + advisory-lock migrations.
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
   - Phase D: Workspace storage + canonical-path uniqueness.
   - Phase E: Workspace switcher modal.
   - Phase F: Open in Cursor / Reveal in Finder.

4. **Orchestrator**
   - Phase A: Top-left fixed single-instance host tab.
   - Phase B: Cross-tab read privileged capability path.
   - Phase C: Gmail send via OAuth delegation pathway.
   - Phase D: Notification surface integration.
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
   - Phase G: Inbox UI (Chinese) + account switcher + attachment list/open-on-demand; explicitly no compose/send UI buttons.

7. **Papers Plugin**
   - Phase A: Papers sidecar scaffold.
   - Phase B: Zotero three-mode access client (with user-overridable SQLite path).
   - Phase C: arxiv-rs query engine + dedup + rule-based recommendations + polite caching.
   - Phase D: Papers tab UI (Chinese) with merged list + offline indicator.

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
| task7 | Frontend lifecycle hooks (`usePluginCapability` issuing branded handle from host; `onUnmount` rotating; `onError`) + host error boundary + capability-rotation race safety (in-flight IPC during rotation either completes atomically or returns `CapabilityExpired`) | AC-1.5 | coding | task4 |
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

### Agreements
- Locked Decisions in v2 draft are absolute constraints; Codex Phase 3 and Phase 5 both confirmed `QUESTIONS_FOR_USER = None`.
- `build.rs` codegen pipeline needs clean-on-build semantics + gitignored generated dirs (Cargo `OUT_DIR` alone is not Vite-friendly).
- MCP stdio sidecars MUST log to stderr/files only; lint rule + integration test catch leakage.
- Per-claude-tab MCP config file approach; cleanup on tab close + startup GC.
- `claude` PATH discovery probes known paths + login-shell PATH; persisted; orchestrator onboarding card if missing.
- Confirm-on-write needs a global queue arbiter (single-modal-at-a-time), not stacked modals.
- Terminal stdin confirmation pauses write but not output/resize/exit observation.
- Scope honesty: confirm-on-write covers plugin-mediated writes only; raw filesystem writes inside workspace are NOT intercepted in MVP.
- Stronghold interrupted-setup recovery via marker file is required.
- SQLite multi-process WAL access needs `busy_timeout` + advisory-lock-guarded migrations to coordinate SIBLING sidecar copies of the same plugin (the real contention scenario).
- Normal-quit sequencing leaves no orphans; force-quit / SIGKILL cannot guarantee cleanup; startup stale-process reaping is the recovery mechanism.
- Structured JSON logs with correlation IDs + redaction, folded into the dispatcher (foundational, not late-stage polish).
- Per-command permission metadata single-sourced from Rust annotations; dispatcher + TS wrappers share metadata.
- Manifest uniqueness checks (duplicate `plugin_id`, `db_namespace`, `command_bin`) enforced at build time.
- Gmail modify operations distinct from send; both are MVP requirements (task31 + task32).
- Gmail tab UI MUST NOT include compose/send buttons in MVP (Phase-2 deferral honored).
- `gmail.delete` tool MUST NOT be registered in MCP tool list.
- Notification permission requested lazily on first event.
- Zotero SQLite path has default + user-overridable.
- 429 backoff ceiling is 5 minutes (avoid effective stall).
- Attachment temp files cleaned within 60 seconds.

### Resolved Disagreements (across 2 convergence rounds)
- **PluginCapability transport** (round 1): replaced "Arc<PluginCapability> in React closure" (impossible across Tauri IPC) with "branded opaque handle / unguessable nonce held in React closure; authoritative state in Rust MountRegistry by `(plugin_id, mount_id)`; validation at every IPC call".
- **Generated dirs wording** (round 1): "checked-in (gitignored)" → "gitignored and regenerated during build". No internal contradiction.
- **AC-1.3 contention test** (round 1): same-plugin sibling sidecars under N×M, not different plugins, is the real concurrent-access scenario; advisory-lock migrations coordinate sibling startup.
- **AC-3.4 testing** (round 1): replaced memory-scanning with deterministic code-path assertion (orchestrator has no `secrets.gmail` permission scope; Stronghold gmail entries inaccessible to orchestrator). Memory scan retained as optional diagnostic only.
- **AC-9.4 force-quit** (round 1): claim only that NORMAL quit guarantees cleanup; force-quit/SIGKILL acknowledged as unable; startup stale-process reaping is the recovery mechanism.
- **Gmail modify ops missing** (round 1): added task32 covering label/archive/mark-read/mark-unread/trash via confirm-on-write.
- **No Gmail compose/send UI in MVP** (round 1): explicitly enforced in task33 description and Allowed Choices.
- **Logging task placement** (round 1): folded into task4 dispatcher (correlation IDs are foundational, not late-stage polish).
- **task6 traceability** (round 1): fixed `AC-4.3, AC-5.4` → `AC-5.2, AC-5.4`.
- **task7 traceability** (rounds 1+2): originally `AC-1.5, AC-6.1` → fixed to `AC-1.5, AC-9.3` (round 1) → simplified to `AC-1.5` (round 2 optional cleanup, since AC-9.3 is more directly served by task5's migration-failure recovery).

### Implementation-Decision Items (NOT user decisions — coding-phase choices)
- MCP Rust SDK: `rmcp` vs hand-written stdio wrapper.
- Type-codegen tool: `ts-rs` vs `specta`.
- Migration runner: `refinery` vs `sqlx::migrate!`.
- React state management: Zustand vs Context+reducer.
- Confirm-on-write queue scope: global (chosen at Lower Bound) vs per-plugin.
- Generated source dir: gitignored + build-regenerated (chosen) vs committed.
- SQLite access pattern: pool per sidecar vs actor-owned connection (decide per-plugin).

### Convergence Status
- Final Status: **`converged`**
- Rounds executed: 2 (Codex round 2 explicitly stated "Convergence is reached. None material" for DISAGREE, REQUIRED_CHANGES, UNRESOLVED.)

## Pending User Decisions

None. Codex Phase 3 and Phase 5 (both convergence rounds) confirmed all architectural decisions are pre-locked via the 6 rounds of user clarification captured in the v2 draft's "Locked Decisions" section. All implementation-decision items (MCP Rust SDK, codegen tool, migration runner, React state, SQLite access pattern) are coding-phase technical choices, not user-facing decisions, and will be made during implementation per the spec's "open implementation decisions" intent.

## Implementation Notes

### Code Style Requirements
- Implementation code and comments must NOT contain plan-specific terminology such as "AC-", "Milestone", "Step", "Phase", or similar workflow markers.
- These terms are for plan documentation only, not for the resulting codebase.
- Use descriptive, domain-appropriate naming in code instead (e.g., the host trait is `PluginHost`, not `MilestoneOnePluginHost`).

### Engineering Threshold Defaults (not user-facing — set by Claude for testability)
The plan introduces several engineering-level timing/size thresholds beyond the user-confirmed HARD/DIRECTIONAL values (≥4 PTYs HARD, ≥2 Gmail HARD, 1 MB ring HARD, ~30 min arXiv DIRECTIONAL). These are reasonable defaults adjustable during implementation if AC tests reveal them unsuitable:
- 5s sidecar graceful-Shutdown timeout before SIGKILL.
- 5min ceiling on Gmail 429 exponential backoff.
- 60s attachment temp-file cleanup window.
- 2s notification dedup window per `(plugin_id, terminal_id, event_kind)`.
- 100-operation SQLite concurrent-access stress test (AC-1.3).
- 5000ms SQLite `busy_timeout`.
- Sidecar auto-restart backoff: 1s → 2s → 4s → … → 60s cap; reset after 5min stable uptime.

### Threat Model Assumptions
- MVP runs all plugin frontends inside a single Tauri WebView; plugin sidecars run as separate OS processes.
- Plugin-vs-plugin isolation rests on (a) `PluginCapability` branded handles held only in component closures (not globally exposed) + (b) Rust `MountRegistry` validation by `(plugin_id, mount_id)` at the IPC dispatcher.
- OS process boundary is the only sandbox layer (no `sandbox-exec` profile, no macOS App Sandbox for MVP).
- Workspace isolation is advisory (cwd-based); a determined adversarial claude could read files outside its workspace. Acceptable for personal-use MVP; Phase-2 sandbox tightening planned.
- Per-plugin WebViews and OS-level sandbox are Phase-2 hardening targets if the threat model tightens.

--- Original Design Draft Start ---

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

--- Original Design Draft End ---
