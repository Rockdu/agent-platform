# Ask Codex Input

## Question

You are doing a ROUND-3 (final) REASONABILITY REVIEW. Claude has revised the candidate plan to address all round-2 REQUIRED_CHANGES (AC vs Lower Bound alignment, generated frontend tab registry, caller-identity model via PluginCapability, Gmail delete clarification). Confirm whether each round-2 required change was correctly applied and identify any remaining REQUIRED_CHANGES. If there are no further required changes, say so explicitly — convergence is reached.

Use the same five-section output format: AGREE / DISAGREE / REQUIRED_CHANGES / OPTIONAL_IMPROVEMENTS / UNRESOLVED. Be terse.

This is round 3 — the maximum convergence rounds. After this we either converge or carry remaining required changes as user decisions.

---

# Candidate Plan v3

# CANDIDATE PLAN v3 — Tauri Plugin-Hosted Personal Agent And Information Workspace (MVP)

(Round-3 candidate after applying Codex round-2 required changes.)

## Goal Description

Build the MVP of a Tauri v2 desktop application that hosts three first-class plugin tabs — Terminal Mesh, Gmail (multi-account), Papers (Zotero + arXiv) — on top of a declarative plugin contract designed so future tabs (WeChat, GitHub PR review, ...) can be added by dropping a new plugin directory (manifest + Rust adapter + frontend) under `plugins/<id>/` and rebuilding, without modifying host source, host routers, or other plugins. Long-running terminal state (completion, needs-attention) surfaces through a macOS menubar tray window plus a native notification, serving as the desktop analog of iOS Dynamic Island.

## Acceptance Criteria

- **AC-1: Plugin contract is declarative and host-agnostic.** Adding a plugin requires only adding a new directory under `plugins/<id>/` containing a `plugin.toml` manifest, a Rust adapter crate implementing the `PluginHost` trait, and a frontend directory implementing the `PluginTab` interface. The host discovers plugins via a build script that scans `plugins/` and generates BOTH the Rust registry table AND the frontend tab registry — no edits to host source, router enums, command match arms, or tab switch statements are required.
  - AC-1.1: Manifest schema is enforced (`name`, `type`, `command`, `frontend`, `permissions`, `requiredApis`, `dbNamespace`).
    - Positive: a valid manifest registers the plugin and surfaces the tab on app launch.
    - Negative: a manifest missing a required field is rejected at build time (build script fails) or at startup with a typed error pointing to the missing field; the app does not crash silently.
  - AC-1.2: Typed IPC wrappers are generated from a Rust single-source-of-truth schema. Frontend code calls plugins via `plugins.<id>.commands.<commandName>(args)`-style generated functions, not raw `invoke('...')` strings.
    - Positive: a TS call to a generated wrapper is type-checked at compile time; wrong arg types fail `tsc`.
    - Negative: calling a non-existent command is a TS compile error; passing args that fail runtime schema validation returns a typed error variant without panicking.
  - AC-1.3: Each plugin owns its own SQLite database file at `${APP_DATA}/plugins/<id>/state.sqlite` with a separate connection pool and migration runner. The host does NOT expose cross-plugin SQL access.
    - Positive: plugin A and plugin B run their own migrations independently; deleting plugin A's `state.sqlite` resets only A.
    - Negative: plugin A cannot open or query plugin B's `state.sqlite` through any host-provided API.
  - AC-1.4: Permission gating is enforced at the IPC boundary (mandatory, not warning-only). The host rejects IPC calls whose command requires a permission the caller's plugin manifest does not declare.
    - Positive: a plugin with `user.email` declared can invoke Gmail-scoped commands.
    - Negative: a plugin without `user.email` attempting a Gmail-scoped command receives a typed `PermissionDenied` error before the command runs.
  - AC-1.5: Frontend plugin interface provides lifecycle hooks and required UI states.
    - Positive: a plugin component implements `onMount` / `onUnmount` / `onError` / empty-state / loading-state hooks; lifecycle is invoked by the host shell at the right moments.
    - Negative: a plugin throwing during `onMount` is caught at the host boundary and renders the plugin's own error state, not a white-screen app crash.
  - AC-1.6: Caller identity is non-spoofable. At plugin mount the host hands each plugin a scoped `PluginCapability` object (containing a per-mount session token). Permissioned IPC calls require that capability and are bound to a specific plugin id at the Rust dispatcher. Other plugins importing the same generated wrapper module cannot impersonate the gated plugin.
    - Positive: a permissioned wrapper invoked from plugin A's component (carrying A's capability) succeeds; the same wrapper invoked from plugin B's component (carrying B's capability) fails with `PermissionDenied`.
    - Negative: a call without a valid `PluginCapability` (e.g., forged or stolen token) is rejected by the dispatcher; tokens rotate on plugin unmount/remount.
  - AC-1.7: A generated frontend tab registry (`src/generated/plugin-tabs.ts`) lists all discovered plugins with lazy-loaded component imports. The host's root router consumes this registry; it does NOT contain a hand-maintained list of plugin IDs.
    - Positive: adding a new plugin directory and rebuilding produces an updated `plugin-tabs.ts` and the new tab appears in the UI with no router edits.
    - Negative: removing a plugin directory and rebuilding removes its tab without breaking the router.

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
    - Negative: a PTY emitting bursts faster than the frontend can render does not OOM the host; the host applies bounded buffering (e.g., 1 MB ring) and drops or coalesces according to a documented policy.

- **AC-3: Notification surface delivers completion and needs-attention alerts without spam.**
  - AC-3.1: A terminal completion event triggers exactly one native notification AND one update to the menubar tray window.
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

- **AC-5: Papers plugin reads Zotero, fetches arXiv, and deduplicates merged results with three distinct access modes.**
  - AC-5.1: Zotero data source uses three modes, in priority order:
    1. **Live**: Zotero local HTTP API at `localhost:23119` (when Zotero desktop is running and the local-API setting is enabled).
    2. **SQLite read-only fallback**: open `zotero.sqlite` with `?mode=ro&immutable=1` (when Zotero is closed but the file is readable).
    3. **App cached snapshot**: the Papers plugin's own materialized cache of the last successful Live or SQLite read (when both above paths fail or are stale).
    - Positive: with Zotero running, the Live path is taken and items reflect current Zotero state.
    - Negative: with Zotero closed AND the SQLite file unreadable (e.g., file lock, permission, missing), the UI shows an "offline — using cached snapshot from <timestamp>" indicator and still renders the last-known items.
  - AC-5.2: arXiv metadata is fetched via `arxiv-rs` and merged into the Papers list with deduplication by DOI/arXiv ID against Zotero items.
    - Positive: an arXiv item already in Zotero appears once with an "in-library" badge, not as two cards.
    - Negative: items that share a substring of title/author but have distinct DOIs are NOT collapsed.
  - AC-5.3: arXiv query/recommendation source is rule-based for MVP (categories + Zotero tag affinity); personalized ML-style ranking is out of MVP scope.
    - Positive: configuring an arXiv category subscription produces fresh items in the tab.
    - Negative: changing the rule does not require restarting the app.
  - AC-5.4: arXiv API access is polite: results are cached, refresh cadence has a minimum interval (default 30 minutes per configured query), and a manual refresh respects a short rate guard.
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

> **Convention used here**: The Lower Bound describes the **minimum implementation that still passes ALL acceptance criteria**. The Upper Bound describes the **most polished implementation that does not over-engineer**. The slack between them is in UI polish, extra UX surfaces, internationalization, and platform reach — NOT in AC-required behaviors.

### Upper Bound (Maximum Acceptable Scope)
- **Plugin contract**: full lifecycle hooks (mount / unmount / error / loading / empty), per-mount `PluginCapability` token rotation, generated Rust dispatcher + TS wrappers + frontend tab registry, refined error surfaces, optional per-plugin telemetry hooks.
- **Terminal Mesh**: ≥4 concurrent PTYs with refined backpressure (coalesce-on-overflow), agent-marker OSC support, prompt-waiting heuristic for shells where it is reliable, polished completion animations.
- **Gmail**: multi-account read + modify (label / archive / mark-read / mark-unread / trash). Permanent delete is NOT included (would require `gmail.delete` scope). Incremental sync with rate-limit backoff curve, attachment list-and-open-on-demand, polished re-auth UX.
- **Papers**: full three-mode Zotero source (Live / SQLite-ro / cached snapshot), arXiv with rule-based recommendations, polite caching, dedup with in-library badge, snapshot-timestamp indicator.
- **Application shell**: bilingual CN/EN UI strings, shared design system across plugins, plugin-scoped recovery UIs, error boundaries at every plugin boundary.
- **Packaging**: macOS code-signing / notarization is **stretch**, not required for local MVP validation.

### Lower Bound (Minimum Acceptable Scope — still passes ALL ACs)
- **Plugin contract**: manifest enforced, generated typed IPC wrappers, generated Rust registry AND frontend tab registry, per-plugin DB files, mandatory permission gating, non-spoofable caller identity via `PluginCapability`, lifecycle hooks including error boundary. (All AC-1 sub-criteria are required.)
- **Terminal Mesh**: ≥4 concurrent PTYs (required by AC-2.1), stdin/resize/exit/cwd/env support, host-managed lifetime with clean teardown, cancellation + bounded buffering. UI may be minimal (no animations, basic tab switcher).
- **Notifications**: BOTH native + tray on completion (required by AC-3.1), dedup window, basic permission-denied banner. UI may be unstyled.
- **Gmail**: ≥2 accounts (required by AC-4.1), incremental sync with historyId and 429 backoff (required by AC-4.2), Stronghold-only tokens (required by AC-4.3). Inbox UI may be minimal — table view with subject/sender/date.
- **Papers**: all three Zotero access modes (required by AC-5.1), DOI/arXiv-ID dedup (required by AC-5.2), at least one rule source (required by AC-5.3), polite caching guard (required by AC-5.4). Recommendation UI may be a simple list.
- **Application shell**: three tabs render with empty states (required by AC-6.1), plugin-scoped corruption recovery (required by AC-6.2). English-only UI strings acceptable at lower bound; bilingual is upper-bound polish.

### Allowed Choices
- **Can use**: Tauri v2; React 18+ / TypeScript; Rust (stable, 2024 edition); `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold` (primary secret store), `rusqlite`; for Gmail: `google-gmail1` (Google REST, preferred); for arXiv: `arxiv-rs`; for Zotero: local HTTP API + SQLite read-only; for IPC type-codegen: `ts-rs` or `specta` or equivalent.
- **Cannot use**: Electron; pure-web SPA without desktop shell; SwiftUI / AppKit-native; tmux-as-system-of-record; notebook-cell UI paradigm; MCP-server-only architecture; raw OS keychain APIs as primary token store (Stronghold is primary; keychain may be a Phase-2 alternative); raw `invoke('...')` strings in plugin code; hand-maintained plugin enumeration in the router.

## Feasibility Hints and Suggestions

### Conceptual Approach

1. **Host skeleton** (Tauri v2 scaffold + React/TS shell + tab router consuming `src/generated/plugin-tabs.ts`) is built first. The shell exposes the `PluginHost` Rust trait and a `PluginTab` TS interface.
2. **Plugin contract** is formalized as: `plugin.toml` manifest schema + Rust traits + a `build.rs` script that scans `plugins/*/plugin.toml` and emits:
   - `src-tauri/src/generated_plugin_registry.rs` — Rust registry + dispatcher.
   - `src/generated/plugins/<id>.ts` — typed IPC wrappers per plugin.
   - `src/generated/plugin-tabs.ts` — frontend tab registry with lazy imports.
3. **Typed IPC code-gen**: use `ts-rs` (or `specta`) on Rust command/event/error types; the build script emits TS files. The generated Rust dispatcher routes IPC calls to the right plugin and enforces permission gating against the call's `PluginCapability`.
4. **Caller-identity model (`PluginCapability`)**: at plugin mount, the host generates a session token bound to `(plugin_id, mount_id)`. The host hands the plugin frontend a `PluginCapability` object via the lifecycle hook. Generated wrappers require the capability as a hidden first parameter. The Rust dispatcher validates the token against its live mount registry and rotates tokens on unmount/remount. Plugin B cannot call plugin A's gated commands because B holds only B's capability.
5. **Per-plugin SQLite**: each plugin owns `${APP_DATA}/plugins/<id>/state.sqlite`. The host hands the plugin a `PluginDb` handle that opens only that file. Migrations are owned by the plugin via `refinery` or `sqlx::migrate!`.
6. **Stronghold**: store OAuth refresh tokens keyed by `(plugin_id, account_id)`. Access tokens stay in process memory only.
7. **Terminal Mesh** wraps `portable-pty` in a Tokio-actor (one task per PTY) emitting `TerminalEvent::Output | Resize | Exit | NeedsAttention`. xterm.js subscribes via the typed event channel. Output buffering is bounded (e.g., 1 MB ring) with coalesce-on-overflow.
8. **Notification surface** is a host-owned service subscribing to plugin events with the `notify` capability; it applies a host-side dedup window (e.g., 2 s per `(plugin_id, event_id)`) before firing native + tray.
9. **Gmail plugin** drives OAuth via the system browser (Tauri URL handler), stores refresh tokens in Stronghold, syncs incrementally via `historyId` with a fallback full-sync on `historyNotFound (404)` and exponential backoff on `429`.
10. **Papers plugin** probes Zotero HTTP API at `localhost:23119` on startup; falls back to SQLite read-only on `zotero.sqlite`; the plugin maintains its own materialized snapshot for the third-mode offline path.

### Specification Deliverables (produced by `analyze`-tagged tasks)
- `docs/specs/plugin-contract.md` — manifest schema, IPC type-gen pipeline, permission enumeration, lifecycle hooks, `PluginCapability` design.
- `docs/specs/terminal-events.md` — needs-attention taxonomy: completion (exit 0), nonzero exit, prompt-waiting heuristic, explicit OSC agent marker. Defines event payload schema and dedup keys. (Written BEFORE notification implementation.)
- `docs/specs/gmail-sync.md` — incremental sync edge cases: `historyNotFound` recovery, 429 backoff curve, watch-vs-poll tradeoff, attachment listing.

### Relevant References

- Tauri v2 architecture: https://v2.tauri.app/concept/architecture/
- Tauri v2 plugin development: https://v2.tauri.app/develop/plugins/
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
   - Phase A: Tauri v2 scaffold + React/TS shell + empty 3-tab router (consuming generated registry).
   - Phase B: Plugin contract spec at `docs/specs/plugin-contract.md` (manifest, IPC pipeline, permissions, lifecycle, `PluginCapability`).
   - Phase C: `build.rs` plugin scanner emitting Rust registry + frontend tab registry + per-plugin TS wrappers.
   - Phase D: Generated Rust IPC dispatcher with permission gating bound to `PluginCapability`.
   - Phase E: Per-plugin SQLite (`${APP_DATA}/plugins/<id>/state.sqlite`) + migration framework.
   - Phase F: tauri-plugin-stronghold integration for OAuth tokens.
   - Phase G: Frontend plugin lifecycle hooks, `PluginCapability` issuance, and error boundary.

2. **Terminal Mesh** — first plugin; stress-tests the contract; unblocks the notification surface.
   - Phase A: Terminal event taxonomy spec at `docs/specs/terminal-events.md` (written BEFORE notification implementation).
   - Phase B: portable-pty Tokio actor with bounded buffer and cancellation.
   - Phase C: xterm.js frontend with multi-tab terminal UI + state-store wiring + concurrent-PTY stress test.
   - Phase D: Notification surface (native + tray window) with host-side dedup/throttle keyed by `(plugin_id, terminal_id, event_kind)`.
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

| Task ID | Description | Target AC | Tag | Depends On |
|---------|-------------|-----------|-----|------------|
| task1 | Scaffold Tauri v2 + React/TS shell with empty 3-tab UI router consuming the generated frontend registry | AC-6.1, AC-1.7 | coding | - |
| task2 | Write plugin contract spec at `docs/specs/plugin-contract.md` (manifest schema, IPC pipeline, permission enumeration, lifecycle hooks, `PluginCapability` design) | AC-1.1, AC-1.2, AC-1.4, AC-1.5, AC-1.6 | analyze | task1 |
| task3 | Implement `build.rs` plugin scanner that emits Rust registry + per-plugin TS wrappers + frontend tab registry (`src/generated/plugin-tabs.ts`) | AC-1.1, AC-1.7 | coding | task2 |
| task4 | Implement typed IPC code-gen pipeline (Rust SoT → TS wrappers via ts-rs or specta) consumed by generated frontend imports | AC-1.2 | coding | task2 |
| task5 | Implement the generated Rust IPC dispatcher with permission gating bound to `PluginCapability` | AC-1.4, AC-1.6 | coding | task3, task4 |
| task6 | Implement per-plugin SQLite file layout (`${APP_DATA}/plugins/<id>/state.sqlite`) and migration framework | AC-1.3 | coding | task1 |
| task7 | Integrate tauri-plugin-stronghold for OAuth refresh-token storage keyed by `(plugin_id, account_id)` | AC-4.3 | coding | task1 |
| task8 | Implement frontend plugin lifecycle hooks (`onMount` issuing `PluginCapability`, `onUnmount` rotating it, `onError`) and host-level error boundary | AC-1.5, AC-1.6, AC-6.1 | coding | task5 |
| task9 | Write terminal event taxonomy spec at `docs/specs/terminal-events.md` (completion / nonzero exit / prompt-waiting / OSC agent marker; explicitly NOT stderr-burst) | AC-3.2 | analyze | task1 |
| task10 | Wrap portable-pty in a Tokio actor with bounded buffer (1 MB ring), cancellation, and `TerminalEvent` stream per the taxonomy spec | AC-2.2, AC-2.4 | coding | task4, task9 |
| task11 | Build xterm.js multi-tab frontend with PTY backend wiring; concurrent-PTY stress test (≥4 PTYs) | AC-2.1, AC-2.3 | coding | task10 |
| task12 | Implement notification surface: native banner + menubar tray window with host-side dedup/throttle keyed by `(plugin_id, terminal_id, event_kind)` | AC-3.1, AC-3.2 | coding | task9, task11 |
| task13 | Handle macOS notification permission denial with in-app fallback banner | AC-3.3 | coding | task12 |
| task14 | Implement Gmail OAuth flow (system browser) with per-account Stronghold storage | AC-4.1 | coding | task7, task5 |
| task15 | Write Gmail sync edge-case spec at `docs/specs/gmail-sync.md` (historyNotFound recovery, 429 backoff curve, watch-vs-poll tradeoff, attachment listing) | AC-4.2 | analyze | task14 |
| task16 | Implement Gmail sync DB + incremental sync engine + rate-limit backoff per the spec | AC-4.2 | coding | task15, task6 |
| task17 | Build Gmail inbox UI + account switcher + re-auth UX + attachment list/open-on-demand | AC-4.1 | coding | task16 |
| task18 | Implement Zotero local HTTP API client (probe `localhost:23119`) with SQLite read-only fallback and app-owned cached snapshot | AC-5.1 | coding | task6 |
| task19 | Implement arxiv-rs query engine + rule-based recommendation + polite caching (default 30 min minimum refresh interval) | AC-5.2, AC-5.3, AC-5.4 | coding | task18 |
| task20 | Build Papers tab UI with merge/dedup against Zotero + offline indicator (timestamp of cached snapshot) | AC-5.1, AC-5.2, AC-5.3 | coding | task19 |
| task21 | Implement plugin-scoped SQLite corruption recovery + per-plugin reset UX | AC-6.2 | coding | task6 |

## Claude-Codex Deliberation

### Agreements
- Plugin contract formalized as: manifest + typed IPC + per-plugin DB files + permission gating + lifecycle hooks + non-spoofable caller identity + generated frontend tab registry. (All AC-1 sub-criteria.)
- PTY lifecycle handled by one Tokio actor per PTY with bounded buffer and cancellation.
- Gmail multi-account requires consistent `account_id` keying across token storage, sync DB, and UI.
- Zotero three-mode access (Live / SQLite-ro / cached snapshot).
- Notification dedup/throttle is host-owned, keyed by `(plugin_id, terminal_id, event_kind)`.
- OAuth refresh tokens live in Stronghold only.
- Compile-time plugin registry (via `build.rs` scanning `plugins/`) for MVP; runtime user-installable plugins are Phase 2.
- IPC schema generated from a Rust SoT — raw `invoke('...')` strings are forbidden in plugin code.
- SQLite isolation via separate DB files per plugin.
- Permission gating is mandatory in the lower bound; warning-only is not acceptable.
- Gmail Upper Bound is read + modify (label / archive / mark / trash); permanent delete (`gmail.delete` scope) is deferred to Phase 2 — Upper Bound and DEC-3/DEC-4 are consistent.
- Stderr-burst is NOT a default needs-attention trigger.
- `analyze`-tagged tasks have concrete spec deliverables.
- Lower Bound describes the minimum implementation that still passes ALL ACs. AC-required behaviors (≥4 PTYs, ≥2 Gmail accounts, three-mode Zotero, native + tray notifications) are not in the slack zone — they are required at the lower bound. Slack lives in UI polish, bilingual layer, and platform reach.
- Caller identity is enforced via per-mount `PluginCapability` tokens; generated wrappers require the capability as a hidden parameter; the Rust dispatcher validates against its live mount registry.
- A generated frontend tab registry (`src/generated/plugin-tabs.ts`) is required so the router never enumerates plugins manually.

### Resolved Disagreements
- **IPC schema generation**: typed wrappers generated from Rust SoT (round-1 fix).
- **SQLite isolation**: per-plugin DB files, not shared connection (round-1 fix).
- **Permission gating posture**: mandatory in lower bound (round-1 fix).
- **Zotero offline modes**: three distinct modes — Live / SQLite-ro / cached snapshot (round-1 fix).
- **Gmail Upper Bound vs DEC-3**: Upper Bound is now read + modify only, no send/reply (round-1 fix). Permanent delete excluded (round-2 clarification).
- **Needs-attention taxonomy**: stderr-burst removed (round-1 fix).
- **Stronghold vs OS keychain**: Stronghold primary (round-1 fix).
- **AC vs Lower Bound alignment**: Lower Bound rewritten so it satisfies ALL ACs; slack lives in UI polish (round-2 fix).
- **Generated frontend tab registry**: required (round-2 fix).
- **Caller identity (`PluginCapability`)**: per-mount session token enforced at Rust dispatcher (round-2 fix).

### Convergence Status
- Round: 3 (after applying round-2 required changes). Awaiting round-3 verification by Codex.

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
  - Claude Position: read + modify (label / archive / mark-read / trash) in MVP; send/reply deferred to Phase 2.
  - Codex Position: agrees with deferring send/reply.
  - Tradeoff Summary: send/reply adds compose UX and draft management which is non-trivial.
  - Decision Status: PENDING

- **DEC-4: Gmail OAuth scopes acceptable.**
  - Claude Position: `gmail.modify` (read + add/remove labels + archive + mark-read/unread + trash). NOT `gmail.delete` (permanent delete deferred to Phase 2). NOT `gmail.send`.
  - Codex Position: agrees, tied to DEC-3.
  - Tradeoff Summary: `gmail.modify` is the minimum scope that covers triage; broader scopes increase blast radius if tokens leak.
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
  - Claude Position: CN/EN bilingual UI strings from day one (Upper Bound); English-only acceptable at Lower Bound.
  - Codex Position: acceptable but not required for MVP unless user-facing Chinese is a product requirement.
  - Tradeoff Summary: bilingual at MVP is cheap if planned now; retrofitting is more expensive.
  - Decision Status: PENDING

- **DEC-12: Data retention policy for terminal scrollback, Gmail cached metadata, and downloaded/opened attachments.**
  - Claude Position: terminal scrollback in-memory only (no persistence); Gmail cached metadata kept indefinitely until plugin reset; attachments not downloaded (open-on-demand only).
  - Codex Position: surfaced as open question; user must confirm.
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
- Timestamp: 2026-05-16_00-14-55
- Tool: codex
