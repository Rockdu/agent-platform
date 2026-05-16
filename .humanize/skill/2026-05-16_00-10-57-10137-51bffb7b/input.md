# Ask Codex Input

## Question

You are doing a ROUND-2 REASONABILITY REVIEW. Claude has revised the candidate plan to address all REQUIRED_CHANGES from the round-1 review and incorporated several OPTIONAL_IMPROVEMENTS. Your job now: confirm whether the round-1 required changes were correctly applied, and identify any NEW required changes that emerged from v1→v2 revisions. Output the SAME five sections as before: AGREE / DISAGREE / REQUIRED_CHANGES / OPTIONAL_IMPROVEMENTS / UNRESOLVED. If there are no further REQUIRED_CHANGES, say so explicitly. Be terse where the answer is 'looks good.'

Scope reminders (locked): Tauri v2 + Rust + React/TS; MVP = Terminal Mesh + Gmail multi-account + Zotero/arXiv (3 tabs); WeChat + GitHub PR review deferred.

---

# Candidate Plan v2

# CANDIDATE PLAN v2 — Tauri Plugin-Hosted Personal Agent And Information Workspace (MVP)

(Round-2 candidate after applying Codex round-1 required changes + selected optional improvements.)

## Goal Description

Build the MVP of a Tauri v2 desktop application that hosts three first-class plugin tabs — Terminal Mesh, Gmail (multi-account), Papers (Zotero + arXiv) — on top of a declarative plugin contract designed so future tabs (WeChat, GitHub PR review, ...) can be added by dropping a new plugin directory (manifest + Rust adapter + frontend) under `plugins/<id>/` and rebuilding, without modifying host source, host routers, or other plugins. Long-running terminal state (completion, needs-attention) surfaces through a macOS menubar tray window plus a native notification, serving as the desktop analog of iOS Dynamic Island.

## Acceptance Criteria

- **AC-1: Plugin contract is declarative and host-agnostic.** Adding a plugin requires only adding a new directory under `plugins/<id>/` containing a `plugin.toml` manifest, a Rust adapter crate implementing the `PluginHost` trait, and a frontend directory implementing the `PluginTab` interface. The host discovers plugins via a build script that scans `plugins/` and generates the registry table — no edits to host source, router enums, command match arms, or tab switch statements are required.
  - AC-1.1: Manifest schema is enforced (`name`, `type`, `command`, `frontend`, `permissions`, `requiredApis`, `dbNamespace`).
    - Positive: a valid manifest registers the plugin and surfaces the tab on app launch.
    - Negative: a manifest missing a required field is rejected at build time (build script fails) or at startup with a typed error pointing to the missing field; the app does not crash silently.
  - AC-1.2: Typed IPC wrappers are generated from a Rust single-source-of-truth schema. Frontend code calls plugins via `plugins.<id>.commands.<commandName>(args)`-style generated functions (e.g. `plugins.gmail.commands.listMessages({ accountId })`), not raw `invoke('...')` strings.
    - Positive: a TS call to a generated wrapper is type-checked at compile time; wrong arg types fail `tsc`.
    - Negative: calling a non-existent command is a TS compile error; passing args that fail runtime schema validation returns a typed error variant without panicking.
  - AC-1.3: Each plugin owns its own SQLite database file at `${APP_DATA}/plugins/<id>/state.sqlite` with a separate connection pool and migration runner. The host does NOT expose cross-plugin SQL access.
    - Positive: plugin A and plugin B run their own migrations independently; deleting plugin A's `state.sqlite` resets only A.
    - Negative: plugin A cannot open or query plugin B's `state.sqlite` through any host-provided API.
  - AC-1.4: Permission gating is enforced at the IPC boundary (mandatory, not warning-only). The host rejects IPC calls whose command requires a permission the calling plugin's manifest does not declare.
    - Positive: a plugin with `user.email` declared can invoke Gmail-scoped commands.
    - Negative: a plugin without `user.email` attempting a Gmail-scoped command receives a typed `PermissionDenied` error before the command runs.
  - AC-1.5: Frontend plugin interface provides lifecycle hooks and required UI states.
    - Positive: a plugin component implements `onMount` / `onUnmount` / `onError` / empty-state / loading-state hooks; lifecycle is invoked by the host shell at the right moments.
    - Negative: a plugin throwing during `onMount` is caught at the host boundary and renders the plugin's own error state, not a white-screen app crash.

- **AC-2: Terminal Mesh supports multiple concurrent PTYs with backpressure-aware streams.**
  - AC-2.1: At least 4 concurrent PTY sessions can be opened simultaneously, each streaming output to its own xterm.js instance.
    - Positive: spawn 4 PTYs running long commands; all four show streaming output without blocking each other.
    - Negative: closing a PTY tab while a command runs cleans up the child process; orphans are not left in `ps aux`.
  - AC-2.2: PTY supports stdin, resize, exit-status reporting, and cwd/env at spawn.
    - Positive: typing into a PTY reaches the child; resizing the xterm.js viewport propagates SIGWINCH; child exit code is visible in the tab UI.
    - Negative: spawning with an invalid shell path returns a typed error and renders an in-tab error state without crashing the app.
  - AC-2.3: Terminal session lifetime is host-managed: sessions terminate on app exit by default; persistence across restart is explicitly out of MVP scope (see DEC-1).
    - Positive: app quit cleanly tears down all PTY child processes within a defined grace period.
    - Negative: after force-kill of the app, no orphan PTY children persist beyond OS process-tree cleanup.
  - AC-2.4: Event streams (PTY output, Gmail sync progress, arXiv polling) support cancellation and backpressure.
    - Positive: closing a PTY tab cancels its output stream within one event loop tick.
    - Negative: a PTY emitting bursts faster than the frontend can render does not OOM the host; the host applies bounded buffering and drops or coalesces according to a documented policy.

- **AC-3: Notification surface delivers completion and needs-attention alerts without spam.**
  - AC-3.1: A terminal completion event triggers exactly one native notification and one update to the menubar tray window.
    - Positive: a long `sleep 10 && echo done` produces one banner + one tray entry on completion.
    - Negative: a tight loop of fast-completing commands produces no more than one notification per `(plugin_id, terminal_id, event_kind)` per dedup window.
  - AC-3.2: The default needs-attention trigger set is: child exit (zero), child exit (nonzero), explicit agent marker via OSC sequence, and prompt-waiting heuristic (if reliably detectable). Stderr-burst is NOT a default trigger.
    - Positive: emitting the agent-marker OSC sequence triggers a tray update flagged as `needs-attention`.
    - Negative: writing a large blob to stderr does not trigger an alert in the default configuration.
  - AC-3.3: macOS notification permission denial degrades gracefully.
    - Positive: launching with notifications denied shows an in-app banner explaining the fallback and continues to drive the tray window.
    - Negative: a denied permission does not cause a runtime panic or repeated permission re-prompts.

- **AC-4: Gmail plugin supports multiple independent accounts with persistent incremental sync.**
  - AC-4.1: At least two Gmail accounts can be added via OAuth and their inboxes are independently visible; account switching is explicit and shows the active account in the UI.
    - Positive: adding two accounts shows both as switchable identities; each shows its own inbox.
    - Negative: revoking account A externally produces a typed re-auth prompt for A without affecting B.
  - AC-4.2: Sync state per account is keyed by account ID; incremental sync uses Gmail `historyId` so restart does not trigger a full re-sync. Rate-limit responses are handled with exponential backoff.
    - Positive: app restart resumes sync from the last `historyId`; only new messages are fetched.
    - Negative: an invalid `historyId` (`historyNotFound`) triggers a controlled re-sync, not a crash; a `429 Too Many Requests` response triggers exponential backoff and a visible "syncing slower" indicator, not a tight retry loop.
  - AC-4.3: OAuth refresh tokens are stored only via tauri-plugin-stronghold (primary path), keyed by `(plugin_id, account_id)`. Tokens NEVER appear in any SQLite file, in WebView localStorage / sessionStorage / IndexedDB, or in plaintext config.
    - Positive: scanning all plugin SQLite files post-OAuth finds zero token strings.
    - Negative: scanning the WebView storage of a running app finds zero token strings.

- **AC-5: Papers plugin reads Zotero, fetches arXiv, and deduplicates merged results with two distinct offline modes.**
  - AC-5.1: Zotero data source uses three modes, in priority order:
    1. **Live**: Zotero local HTTP API at `localhost:23119` (when Zotero desktop is running and the local-API setting is enabled).
    2. **SQLite read-only fallback**: open `zotero.sqlite` with `?mode=ro&immutable=1` (when Zotero is closed but the file is readable).
    3. **App cached snapshot**: the Papers plugin's own materialized cache of the last successful Live or SQLite read (when both above paths fail or are stale).
    - Positive: with Zotero running, the Live path is taken and items reflect current Zotero state.
    - Negative: with Zotero closed and the SQLite file unreadable (e.g., file lock, permission), the UI shows an "offline — using cached snapshot from <timestamp>" indicator and still renders the last-known items.
  - AC-5.2: arXiv metadata is fetched via `arxiv-rs` and merged into the Papers list with deduplication by DOI/arXiv ID against Zotero items.
    - Positive: an arXiv item already in Zotero appears once with an "in-library" badge, not as two cards.
    - Negative: items that share a substring of title/author but have distinct DOIs are NOT collapsed.
  - AC-5.3: arXiv query/recommendation source is rule-based for MVP (categories + Zotero tag affinity); personalized ML-style ranking is out of MVP scope.
    - Positive: configuring an arXiv category subscription produces fresh items in the tab.
    - Negative: changing the rule does not require restarting the app.
  - AC-5.4: arXiv API access is polite: results are cached, refresh cadence has a minimum interval (default 30 minutes for each configured query), and a manual refresh respects a short rate guard.
    - Positive: rapid clicks on "refresh" do not exceed the rate guard.
    - Negative: the app does not poll arXiv more often than the configured minimum interval per query.

- **AC-6: Application shell handles cold-start and corruption paths.**
  - AC-6.1: Three MVP tabs render on first launch with empty/placeholder state; no single plugin failure prevents the others from rendering.
    - Positive: first launch (no DB, no tokens) shows three tabs, each with its own onboarding/empty state.
    - Negative: Gmail plugin failing to initialize does not prevent Terminal Mesh and Papers from rendering.
  - AC-6.2: SQLite corruption or missing files trigger plugin-scoped recovery UI; the host does not crash.
    - Positive: deleting a single plugin's DB and relaunching shows that plugin's "fresh state" onboarding.
    - Negative: corrupting one plugin's DB does not break other plugins.

## Path Boundaries

### Upper Bound (Maximum Acceptable Scope)
The MVP ships three polished plugin tabs:
- **Terminal Mesh**: ≥4 concurrent PTYs, full lifecycle, dedup-aware notifications, backpressure-aware streams, agent-marker OSC support.
- **Gmail**: multi-account read + modify (label/archive/mark-read), incremental sync with rate-limit backoff, attachment list-and-open-on-demand. No send/reply (deferred — see DEC-3).
- **Papers**: Zotero three-mode source (HTTP / SQLite-ro / cached snapshot), arXiv with rule-based recommendations and polite caching, dedup by DOI/arXiv ID, in-library badge.

Plus: declarative plugin contract (manifest + typed IPC + per-plugin DB files + enforced permission gating + frontend lifecycle hooks + Stronghold for secrets), dedup-aware menubar tray + native notification surface, plugin-scoped recovery UIs, shared design system across plugins, and a bilingual CN/EN UI string layer. macOS code-signing / notarization is **stretch**, not required for local MVP validation.

### Lower Bound (Minimum Acceptable Scope)
All three MVP tabs render and validate the plugin contract end-to-end. Plugin contract enforces: manifest schema, generated typed IPC wrappers, per-plugin DB files, **mandatory permission gating** (minimal scope but enforced; warning-only is NOT acceptable), and frontend lifecycle hooks. Terminal Mesh supports ≥2 concurrent PTYs with streaming/resize/input/exit and one notification path (native OR tray). Gmail supports ≥1 account read-only with tokens in Stronghold and incremental sync via `historyId`. Papers supports ≥1 Zotero source mode (Live OR SQLite-ro, not necessarily both) + arXiv listing without recommendations; dedup against Zotero. CN/EN bilingual layer may be deferred to post-MVP.

### Allowed Choices
- **Can use**: Tauri v2; React 18+ / TypeScript; Rust (stable, 2024 edition); `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold` (primary secret store), `rusqlite`; for Gmail: `google-gmail1` (Google REST, preferred); for arXiv: `arxiv-rs`; for Zotero: local HTTP API + SQLite read-only; for IPC type-codegen: `ts-rs` or `specta` or equivalent.
- **Cannot use**: Electron; pure-web SPA without desktop shell; SwiftUI / AppKit-native; tmux-as-system-of-record; notebook-cell UI paradigm; MCP-server-only architecture; raw OS keychain APIs as primary token store (Stronghold is primary; keychain may be a Phase-2 alternative).

## Feasibility Hints and Suggestions

### Conceptual Approach

1. **Host skeleton** (Tauri v2 scaffold + React/TS shell + tab router) is built first. The shell exposes the `PluginHost` Rust trait and a `PluginTab` TS interface.
2. **Plugin contract** is formalized as: `plugin.toml` manifest schema + Rust traits + a `build.rs` script that scans `plugins/*/plugin.toml` and emits `src-tauri/src/generated_plugin_registry.rs` (Rust) and `src/generated/plugins/<id>.ts` (TS wrappers). Adding a plugin = adding a directory under `plugins/`, never editing host source.
3. **Typed IPC code-gen**: use `ts-rs` (or `specta`) on Rust command/event/error types; the build script emits TS files that re-export typed wrappers consumed as `import { commands } from '@/generated/plugins/gmail'`.
4. **Per-plugin SQLite**: each plugin owns `${APP_DATA}/plugins/<id>/state.sqlite`. The host hands the plugin a `PluginDb` handle that opens only that file. Migrations are owned by the plugin via `refinery` or `sqlx::migrate!`.
5. **Stronghold**: store OAuth refresh tokens keyed by `(plugin_id, account_id)`. Access tokens stay in process memory only.
6. **Terminal Mesh** wraps `portable-pty` in a Tokio-actor (one task per PTY) emitting `TerminalEvent::Output | Resize | Exit | NeedsAttention`. xterm.js subscribes via the typed event channel. Output buffering is bounded (e.g., 1 MB ring) with coalesce-on-overflow.
7. **Notification surface** is a host-owned service subscribing to plugin events with the `notify` capability; it applies a host-side dedup window (e.g., 2 s per `(plugin_id, event_id)`) before firing native + tray.
8. **Gmail plugin** drives OAuth via the system browser (Tauri URL handler), stores refresh tokens in Stronghold, syncs incrementally via `historyId` with a fallback full-sync on `historyNotFound (404)` and exponential backoff on `429`.
9. **Papers plugin** probes Zotero HTTP API at `localhost:23119` on startup; falls back to SQLite read-only on `zotero.sqlite`; the plugin maintains its own materialized snapshot for the third-mode offline path.

### Specification Deliverables (for the `analyze`-tagged tasks)
- `docs/specs/terminal-events.md` — needs-attention taxonomy: completion (exit 0), nonzero exit, prompt-waiting heuristic, explicit OSC agent marker. Defines event payload schema and dedup keys.
- `docs/specs/gmail-sync.md` — incremental sync edge cases: `historyNotFound` recovery, rate-limit backoff curve, watch vs poll tradeoffs for MVP, attachment listing.
- `docs/specs/plugin-contract.md` — manifest schema, IPC type-gen pipeline, permission enumeration, lifecycle hooks.

### Relevant References

- Tauri v2 architecture: https://v2.tauri.app/concept/architecture/
- Tauri plugin development: https://v2.tauri.app/develop/plugins/
- xterm.js: https://github.com/xtermjs/xterm.js
- portable-pty: https://docs.rs/portable-pty
- google-gmail1 crate: https://docs.rs/google-gmail1
- arxiv-rs: https://docs.rs/arxiv-rs
- Zotero direct SQLite: https://www.zotero.org/support/dev/client_coding/direct_sqlite_database_access
- Zotero Web API v3: https://www.zotero.org/support/dev/web_api/v3/basics
- Working precedent — Terax AI (Tauri v2 + xterm.js + portable-pty): https://github.com/crynta/terax-ai
- ts-rs: https://github.com/Aleph-Alpha/ts-rs
- specta: https://github.com/oscartbeaumont/specta

## Dependencies and Sequence

### Milestones

1. **Plugin Contract Foundation** — establishes the host before any plugin is written.
   - Phase A: Tauri v2 scaffold + React/TS shell + empty 3-tab router.
   - Phase B: `plugin.toml` manifest schema + `build.rs` scanner that generates the Rust registry table.
   - Phase C: Typed IPC code-gen pipeline (Rust SoT → TS wrappers via ts-rs/specta).
   - Phase D: Per-plugin SQLite (`${APP_DATA}/plugins/<id>/state.sqlite`) + migration framework.
   - Phase E: tauri-plugin-stronghold integration for OAuth tokens.
   - Phase F: Permission gating in the IPC layer (mandatory enforcement).
   - Phase G: Frontend plugin lifecycle hooks and error boundary.

2. **Terminal Mesh** — first plugin; stress-tests the contract; unblocks the notification surface.
   - Phase A: portable-pty Tokio actor with bounded buffer and cancellation.
   - Phase B: xterm.js frontend with multi-tab terminal UI + state-store wiring.
   - Phase C: Terminal event taxonomy spec at `docs/specs/terminal-events.md`.
   - Phase D: Notification surface (native + tray window) with host-side dedup/throttle.
   - Phase E: Permission-denial graceful fallback.

3. **Gmail Plugin** — depends on Stronghold and typed IPC.
   - Phase A: OAuth flow per account + Stronghold token storage.
   - Phase B: Gmail sync edge-case spec at `docs/specs/gmail-sync.md`.
   - Phase C: Account-keyed sync DB + historyId-based incremental sync + rate-limit backoff.
   - Phase D: Inbox UI + account switcher + revoke/re-auth UX + attachment list/open.

4. **Papers Plugin (Zotero + arXiv)** — depends on the SQLite framework.
   - Phase A: Zotero local HTTP API probe + client (Live mode).
   - Phase B: SQLite read-only fallback (`zotero.sqlite` with `?mode=ro&immutable=1`).
   - Phase C: App-owned cached snapshot (third-mode offline path).
   - Phase D: arxiv-rs query engine + rule-based recommendation logic + polite caching.
   - Phase E: Merged Papers list UI with dedup + offline indicator.

Dependencies (relative, not time-based):
- Milestone 1 must be largely complete before Milestone 2 starts.
- Milestone 2 feeds back into Milestone 1 by stress-testing the contract; small Milestone-1 revisions are expected during Milestone 2.
- Milestones 3 and 4 can proceed in parallel once Milestone 1 stabilizes and Milestone 2 has shaken out the IPC/DB/Stronghold patterns.

## Task Breakdown

| Task ID | Description | Target AC | Tag (`coding`/`analyze`) | Depends On |
|---------|-------------|-----------|---------------------------|------------|
| task1 | Scaffold Tauri v2 + React/TS shell with empty 3-tab UI router | AC-6.1 | coding | - |
| task2 | Write plugin contract spec at `docs/specs/plugin-contract.md` (manifest schema, IPC pipeline, permission enumeration, lifecycle hooks) | AC-1.1, AC-1.2, AC-1.4, AC-1.5 | analyze | task1 |
| task3 | Implement `build.rs` plugin scanner that generates the Rust registry from `plugins/*/plugin.toml` | AC-1.1 | coding | task2 |
| task4 | Implement typed IPC code-gen (Rust SoT → TS wrappers via ts-rs or specta) and generated frontend imports | AC-1.2 | coding | task2 |
| task5 | Implement per-plugin SQLite file layout (`${APP_DATA}/plugins/<id>/state.sqlite`) and migration framework | AC-1.3 | coding | task1 |
| task6 | Integrate tauri-plugin-stronghold for OAuth refresh-token storage keyed by `(plugin_id, account_id)` | AC-4.3 | coding | task1 |
| task7 | Implement IPC-layer permission gating (mandatory enforcement) per declared manifest permissions | AC-1.4 | coding | task3, task4 |
| task8 | Implement frontend plugin lifecycle hooks (`onMount`/`onUnmount`/`onError`) and error boundary at the host shell | AC-1.5, AC-6.1 | coding | task1 |
| task9 | Wrap portable-pty in a Tokio actor with bounded buffer, cancellation, and `TerminalEvent` stream | AC-2.2, AC-2.4 | coding | task4 |
| task10 | Build xterm.js multi-tab frontend with PTY backend wiring; concurrent-PTY stress test (≥4 PTYs) | AC-2.1, AC-2.3 | coding | task9 |
| task11 | Write terminal event taxonomy spec at `docs/specs/terminal-events.md` (completion / nonzero exit / prompt-waiting / OSC agent marker; explicitly NOT stderr-burst) | AC-3.2 | analyze | task10 |
| task12 | Implement notification surface: native banner + menubar tray window with host-side dedup/throttle keyed by `(plugin_id, terminal_id, event_kind)` | AC-3.1, AC-3.2 | coding | task11 |
| task13 | Handle macOS notification permission denial with in-app fallback banner | AC-3.3 | coding | task12 |
| task14 | Implement Gmail OAuth flow (system browser) with per-account Stronghold storage | AC-4.1 | coding | task6, task7 |
| task15 | Write Gmail sync edge-case spec at `docs/specs/gmail-sync.md` (historyNotFound recovery, 429 backoff curve, watch-vs-poll tradeoff, attachment listing) | AC-4.2 | analyze | task14 |
| task16 | Implement Gmail sync DB + incremental sync engine + rate-limit backoff per the spec | AC-4.2 | coding | task15, task5 |
| task17 | Build Gmail inbox UI + account switcher + re-auth UX + attachment list/open-on-demand | AC-4.1 | coding | task16 |
| task18 | Implement Zotero local HTTP API client (probe `localhost:23119`) with SQLite read-only fallback and app-owned cached snapshot | AC-5.1 | coding | task5 |
| task19 | Implement arxiv-rs query engine + rule-based recommendation + polite caching (default 30 min minimum refresh interval) | AC-5.2, AC-5.3, AC-5.4 | coding | task18 |
| task20 | Build Papers tab UI with merge/dedup against Zotero + offline indicator (timestamp of cached snapshot) | AC-5.1, AC-5.2, AC-5.3 | coding | task19 |
| task21 | Implement plugin-scoped SQLite corruption recovery + per-plugin reset UX | AC-6.2 | coding | task5 |

## Claude-Codex Deliberation

### Agreements
- Plugin contract must be formalized (manifest + typed IPC + per-plugin DB files + permission gating + lifecycle hooks) before any plugin is written.
- PTY lifecycle is high-risk; one Tokio actor per PTY with bounded buffer and cancellation is the right shape.
- Gmail multi-account requires consistent `account_id` keying across token storage, sync DB, and UI.
- Zotero strategy: HTTP API primary, SQLite read-only fallback, app-owned cached snapshot as the third (offline-offline) mode.
- Notification dedup/throttle is host-owned, not plugin-owned.
- OAuth refresh tokens live in Stronghold only; SQLite holds non-secret metadata.
- Compile-time plugin registry (via `build.rs` scanning `plugins/`) is realistic for MVP; runtime user-installable plugins are Phase 2.
- IPC schema must be generated from a Rust SoT — raw `invoke('...')` strings are not type-safe.
- SQLite isolation must be enforced via separate DB files, not table-prefixing on a shared connection.
- Permission gating is mandatory in the lower bound; warning-only is not acceptable.
- Gmail Upper Bound is read + modify (not send/reply); send/reply is deferred to Phase 2 — Upper Bound and DEC-3 are now consistent.
- Stderr-burst is NOT a default needs-attention trigger.
- `analyze`-tagged tasks have concrete deliverables (named spec docs), not vague "consult Codex" language.

### Resolved Disagreements (Claude positions, both sides now agree)
- **IPC schema generation**: typed wrappers generated from Rust SoT (ts-rs/specta), exposed as `plugins.<id>.commands.<name>(args)`. Round-1 Codex required this fix; v2 incorporates it.
- **SQLite isolation**: per-plugin DB files at `${APP_DATA}/plugins/<id>/state.sqlite`, not ATTACHed namespaces. Round-1 Codex required this fix; v2 incorporates it.
- **Permission gating posture**: mandatory in lower bound. Round-1 Codex required this fix; v2 incorporates it.
- **Zotero offline modes**: three distinct modes (Live / SQLite-ro / cached snapshot) — round-1 Codex required separating SQLite-fallback from cached-snapshot; v2 incorporates it.
- **Gmail Upper Bound vs DEC-3**: Upper Bound is now read + modify only, consistent with DEC-3. Round-1 Codex flagged the contradiction; v2 resolves it.
- **Needs-attention taxonomy**: stderr-burst removed from defaults; round-1 Codex required this fix; v2 incorporates it.
- **Stronghold vs OS keychain**: Stronghold is primary; OS keychain is Phase-2 alternative. Codex flagged the ambiguity; v2 picks one.

### Convergence Status
- Round: 2 (after applying round-1 required changes). Pending round-2 verification.

## Pending User Decisions

- **DEC-1: Terminal session persistence across app restart.**
  - Claude Position: out of MVP scope.
  - Codex Position: agrees (default out of MVP).
  - Tradeoff Summary: persistence requires either a session daemon (tmux-style) or PTY-state serialization; both add packaging complexity.
  - Decision Status: PENDING

- **DEC-2: Exact terminal events that trigger 灵动岛-style alert.**
  - Claude Position: child exit (zero), child exit (nonzero), prompt-waiting heuristic (if reliably detectable), explicit OSC agent marker. NOT stderr-burst.
  - Codex Position: agrees with this default set.
  - Tradeoff Summary: too few triggers → users miss state; too many → notification fatigue.
  - Decision Status: PENDING

- **DEC-3: Gmail MVP scope — read-only vs read+modify vs read+send.**
  - Claude Position: read + modify (label/archive/mark-read) in MVP; send/reply deferred to Phase 2.
  - Codex Position: agrees with deferring send/reply.
  - Tradeoff Summary: send/reply adds compose UX and draft management which is non-trivial.
  - Decision Status: PENDING

- **DEC-4: Gmail OAuth scopes acceptable.**
  - Claude Position: `gmail.modify` (read + label + archive + delete; no send).
  - Codex Position: agrees if DEC-3 stays at read+modify; otherwise narrower scope.
  - Tradeoff Summary: tied to DEC-3.
  - Decision Status: PENDING

- **DEC-5: Attachment handling in Gmail.**
  - Claude Position: list attachments inline with the message; open-on-demand via the system default app; no auto-download or indexing in MVP.
  - Codex Position: agrees.
  - Tradeoff Summary: indexing costs DB space and processing; on-demand keeps MVP simple.
  - Decision Status: PENDING

- **DEC-6: Does Papers MVP require the Zotero desktop app to be running?**
  - Claude Position: no — three-mode access (Live / SQLite-ro / cached snapshot) ensures graceful offline.
  - Codex Position: agrees with three distinct modes; user must confirm offline behavior expectations.
  - Tradeoff Summary: three modes add code; single mode forces user to keep Zotero running.
  - Decision Status: PENDING

- **DEC-7: arXiv recommendations — rule-based vs personalized ranking.**
  - Claude Position: rule-based for MVP (categories + Zotero tag affinity); personalized ranking deferred.
  - Codex Position: agrees.
  - Tradeoff Summary: personalized ranking needs an embedding pipeline.
  - Decision Status: PENDING

- **DEC-8: Cross-plugin global search (e.g., search across Gmail + Zotero + terminal scrollback).**
  - Claude Position: out of MVP scope.
  - Codex Position: agrees.
  - Tradeoff Summary: cross-plugin index is a non-trivial schema decision; defer until plugin contract has matured.
  - Decision Status: PENDING

- **DEC-9: Plugin install model — build-time vs user-installable.**
  - Claude Position: build-time only for MVP (via `plugins/<id>/` directory + `build.rs` scan); user-installable is Phase 2.
  - Codex Position: agrees.
  - Tradeoff Summary: user-installable plugins require capability sandboxing and signing.
  - Decision Status: PENDING

- **DEC-10: Local-DB encryption posture — encrypted DBs everywhere, or only secrets in Stronghold.**
  - Claude Position: only secrets in Stronghold; plain SQLite for non-secret state (FileVault covers at-rest).
  - Codex Position: agrees as default; flags this as user-decidable if Gmail/Papers cached content is considered sensitive.
  - Tradeoff Summary: full DB encryption adds key-management complexity.
  - Decision Status: PENDING

- **DEC-11: I18n posture — English-only first, or CN/EN mixed from day one.**
  - Claude Position: CN/EN bilingual UI strings from day one.
  - Codex Position: acceptable but not required for MVP unless user-facing Chinese is a product requirement.
  - Tradeoff Summary: bilingual at MVP is cheap if planned now; retrofitting is more expensive.
  - Decision Status: PENDING

- **DEC-12: Data retention policy for terminal scrollback, Gmail cached metadata, and downloaded/opened attachments.**
  - Claude Position: terminal scrollback in-memory only (no persistence), Gmail cached metadata kept indefinitely until plugin reset, attachments not downloaded (open-on-demand only).
  - Codex Position: N/A — surfaced as an open question; user must confirm.
  - Tradeoff Summary: retention defaults affect disk usage and privacy posture.
  - Decision Status: PENDING

## Implementation Notes

### Code Style Requirements
- Implementation code and comments must NOT contain plan-specific terminology such as "AC-", "Milestone", "Step", "Phase", or similar workflow markers.
- These terms are for plan documentation only, not for the resulting codebase.
- Use descriptive, domain-appropriate naming in code instead (e.g., the host trait is `PluginHost`, not `MilestoneOnePluginHost`).

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_00-10-57
- Tool: codex
