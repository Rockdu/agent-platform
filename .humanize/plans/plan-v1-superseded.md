# Tauri Plugin-Hosted Personal Agent And Information Workspace — MVP

## Goal Description

Build the MVP of a Tauri v2 desktop application that hosts three first-class plugin tabs — Terminal Mesh, Gmail (multi-account), Papers (Zotero + arXiv) — on top of a declarative plugin contract designed so future tabs (WeChat, GitHub PR review, ...) can be added by dropping a new plugin directory (manifest + Rust adapter + frontend) under `plugins/<id>/` and rebuilding, without modifying host source, host routers, or other plugins. Long-running terminal state (completion, needs-attention) surfaces through a macOS menubar tray window plus a native notification, serving as the desktop analog of iOS Dynamic Island.

## Acceptance Criteria

Following TDD philosophy, each criterion includes positive and negative tests for deterministic verification. Quantitative thresholds are marked as `HARD` (must be met exactly) or `DIRECTIONAL` (target where same-order-of-magnitude is acceptable) based on user confirmation.

- **AC-1: Plugin contract is declarative and host-agnostic.** Adding a plugin requires only adding a new directory under `plugins/<id>/` containing a `plugin.toml` manifest, a Rust adapter crate implementing the `PluginHost` trait, and a frontend directory implementing the `PluginTab` interface. The host discovers plugins via a build script that scans `plugins/` and generates BOTH the Rust registry table AND the frontend tab registry — no edits to host source, router enums, command match arms, or tab switch statements are required.
  - AC-1.1: Manifest schema is enforced (`name`, `type`, `command`, `frontend`, `permissions`, `requiredApis`, `dbNamespace`).
    - Positive Tests (expected to PASS):
      - A valid manifest registers the plugin and surfaces the tab on app launch.
      - A manifest with `permissions: ["user.email"]` produces a registry entry that records the declared permission.
    - Negative Tests (expected to FAIL):
      - A manifest missing a required field is rejected at build time (build script fails) or at startup with a typed error pointing to the missing field; the app does not crash silently.
      - A manifest declaring an unknown `requiredApis` value is rejected with a typed error naming the unknown identifier.
  - AC-1.2: Typed IPC wrappers are generated from a Rust single-source-of-truth schema. Frontend code calls plugins via `plugins.<id>.commands.<commandName>(args)`-style generated functions, not raw `invoke('...')` strings.
    - Positive Tests:
      - A TS call to a generated wrapper is type-checked at compile time; correct args produce a typed response.
      - Wrong arg types fail `tsc` before runtime.
    - Negative Tests:
      - Calling a non-existent command is a TS compile error.
      - Passing args that fail runtime schema validation returns a typed error variant without panicking.
  - AC-1.3: Each plugin owns its own SQLite database file at `${APP_DATA}/plugins/<id>/state.sqlite` with a separate connection pool and migration runner. The host does NOT expose cross-plugin SQL access.
    - Positive Tests:
      - Plugin A and plugin B run their own migrations independently; deleting plugin A's `state.sqlite` resets only A.
      - Each plugin's `PluginDb` handle opens only its own file.
    - Negative Tests:
      - Plugin A cannot open or query plugin B's `state.sqlite` through any host-provided API.
      - Attempting cross-namespace SQL via the host API returns a typed access error.
  - AC-1.4: Permission gating is enforced at the IPC boundary (mandatory, not warning-only). The host rejects IPC calls whose command requires a permission the caller's plugin manifest does not declare.
    - Positive Tests:
      - A plugin with `user.email` declared can invoke Gmail-scoped commands.
    - Negative Tests:
      - A plugin without `user.email` attempting a Gmail-scoped command receives a typed `PermissionDenied` error before the command runs.
      - The error is structured (carries plugin id + missing permission), not a generic exception.
  - AC-1.5: Frontend plugin interface provides lifecycle hooks and required UI states.
    - Positive Tests:
      - A plugin component implements `onMount` / `onUnmount` / `onError` / empty-state / loading-state hooks; lifecycle is invoked by the host shell at the right moments.
      - A plugin's empty state renders when its data store returns no rows.
    - Negative Tests:
      - A plugin throwing during `onMount` is caught at the host boundary and renders the plugin's own error state, not a white-screen app crash.
  - AC-1.6: Caller identity is non-spoofable. At plugin mount the host hands each plugin a scoped `PluginCapability` object (per-mount session token). The capability is opaque (not serializable to JSON, not exposed on any global, held only in the plugin component's closure/context). Permissioned IPC calls require that capability and are bound to a specific `(plugin_id, mount_id)` at the Rust dispatcher.
    - Positive Tests:
      - A permissioned wrapper invoked from plugin A's component (carrying A's capability) succeeds.
      - The same wrapper invoked from plugin B's component (carrying B's capability) fails with `PermissionDenied`.
    - Negative Tests:
      - Calls carrying a forged token, an expired token (post-unmount), a token bound to a different plugin, or no token are rejected by the dispatcher.
      - Tokens rotate on plugin unmount/remount; an old token after remount is rejected.
      - The capability object cannot be obtained from a global/window or via DOM inspection.
  - AC-1.7: A generated frontend tab registry (`src/generated/plugin-tabs.ts`) lists all discovered plugins with lazy-loaded component imports. The host's root router consumes this registry; it does NOT contain a hand-maintained list of plugin IDs.
    - Positive Tests:
      - Adding a new plugin directory and rebuilding produces an updated `plugin-tabs.ts` and the new tab appears in the UI with no router edits.
      - Lazy imports keep the initial bundle size bounded — a never-opened plugin's code is not loaded.
    - Negative Tests:
      - Removing a plugin directory and rebuilding removes its tab without breaking the router.

- **AC-2: Terminal Mesh supports multiple concurrent PTYs with backpressure-aware streams.**
  - AC-2.1: At least **4 concurrent PTY sessions** [HARD requirement, confirmed by user] can be opened simultaneously, each streaming output to its own xterm.js instance.
    - Positive Tests:
      - Spawn 4 PTYs running long commands (e.g., `tail -f /var/log/system.log`); all four show streaming output without blocking each other.
      - Spawning a 5th and 6th PTY succeeds.
    - Negative Tests:
      - Closing a PTY tab while a command runs cleans up the child process; orphans are not left in `ps aux` after a defined grace period.
      - Spawning beyond a configured cap (if any) returns a typed error, not a silent failure.
  - AC-2.2: PTY supports stdin, resize, exit-status reporting, and cwd/env at spawn.
    - Positive Tests:
      - Typing into a PTY reaches the child process; resizing the xterm.js viewport propagates SIGWINCH; child exit code is visible in the tab UI.
      - A PTY spawned with `cwd=/tmp` reports `/tmp` as its working directory in `pwd`.
    - Negative Tests:
      - Spawning with an invalid shell path returns a typed error and renders an in-tab error state without crashing the app.
      - Sending input after the child exits returns a typed error variant, not a panic.
  - AC-2.3: Terminal session lifetime is host-managed: sessions terminate on app exit by default; persistence across restart is explicitly out of MVP scope (see DEC-1).
    - Positive Tests:
      - App quit cleanly tears down all PTY child processes within a defined grace period.
    - Negative Tests:
      - After force-kill of the app, no orphan PTY children persist beyond OS process-tree cleanup.
  - AC-2.4: Event streams (PTY output, Gmail sync progress, arXiv polling) support cancellation and backpressure with a **1 MB ring buffer** per PTY [HARD requirement, confirmed by user].
    - Positive Tests:
      - Closing a PTY tab cancels its output stream within one event loop tick.
      - Output produced faster than the frontend can render is bounded to the ring; oldest bytes are coalesced or dropped per a documented policy.
    - Negative Tests:
      - A PTY emitting bursts faster than the frontend can render does not OOM the host.
      - Cancellation does not leak memory across many open-and-close cycles.

- **AC-3: Notification surface delivers completion and needs-attention alerts without spam.**
  - AC-3.1: A terminal completion event triggers exactly one native notification AND one update to the menubar tray window.
    - Positive Tests:
      - A long `sleep 10 && echo done` produces one banner + one tray entry on completion.
      - Two completion events from two different PTYs produce two distinct entries.
    - Negative Tests:
      - A tight loop of fast-completing commands produces no more than one notification per `(plugin_id, terminal_id, event_kind)` per dedup window.
  - AC-3.2: The default needs-attention trigger set is: child exit (zero), child exit (nonzero), explicit agent marker via OSC sequence, and prompt-waiting heuristic (if reliably detectable). Stderr-burst is NOT a default trigger.
    - Positive Tests:
      - Emitting the agent-marker OSC sequence triggers a tray update flagged as `needs-attention`.
      - A nonzero exit code shows distinct visual treatment from a zero exit in the tray.
    - Negative Tests:
      - Writing a large blob to stderr does not trigger an alert in the default configuration.
      - Rapid alternating events do not flicker the tray window; throttle is applied.
  - AC-3.3: macOS notification permission denial degrades gracefully.
    - Positive Tests:
      - Launching with notifications denied shows an in-app banner explaining the fallback and continues to drive the tray window.
    - Negative Tests:
      - A denied permission does not cause a runtime panic or repeated permission re-prompts.

- **AC-4: Gmail plugin supports multiple independent accounts with persistent incremental sync.**
  - AC-4.1: At least **2 Gmail accounts** [HARD requirement, confirmed by user] can be added via OAuth and their inboxes are independently visible; account switching is explicit and shows the active account in the UI.
    - Positive Tests:
      - Adding two accounts shows both as switchable identities; each shows its own inbox.
      - Account switching is reflected within one render cycle.
    - Negative Tests:
      - Revoking account A externally produces a typed re-auth prompt for A without affecting B.
      - Adding the same account twice deduplicates (or shows a typed error), not silent duplication.
  - AC-4.2: Sync state per account is keyed by account ID; incremental sync uses Gmail `historyId` so restart does not trigger a full re-sync. Rate-limit responses are handled with exponential backoff.
    - Positive Tests:
      - App restart resumes sync from the last `historyId`; only new messages are fetched.
      - A `429 Too Many Requests` response triggers exponential backoff and a visible "syncing slower" indicator, not a tight retry loop.
    - Negative Tests:
      - An invalid `historyId` (`historyNotFound`) triggers a controlled re-sync, not a crash.
      - Backoff does not exceed a documented ceiling (avoid effective stall).
  - AC-4.3: OAuth refresh tokens are stored only via `tauri-plugin-stronghold` (primary path), keyed by `(plugin_id, account_id)`. Tokens NEVER appear in any SQLite file, in WebView localStorage / sessionStorage / IndexedDB, or in plaintext config.
    - Positive Tests:
      - Scanning all plugin SQLite files post-OAuth finds zero token strings.
      - Tokens are retrievable only via Stronghold unlock.
    - Negative Tests:
      - Scanning the WebView storage of a running app finds zero token strings.
      - No plaintext token appears in any `~/Library/Application Support/<app>/**` file.

- **AC-5: Papers plugin reads Zotero, fetches arXiv, and deduplicates merged results with three distinct access modes.**
  - AC-5.1: Zotero data source uses three modes, in priority order: (1) **Live** — Zotero local HTTP API at `localhost:23119` when Zotero desktop is running and the local-API setting is enabled; (2) **SQLite read-only fallback** — open `zotero.sqlite` with `?mode=ro&immutable=1` when Zotero is closed but the file is readable; (3) **App cached snapshot** — the Papers plugin's own materialized cache when both above paths fail or are stale.
    - Positive Tests:
      - With Zotero running, the Live path is taken and items reflect current Zotero state.
      - With Zotero closed but SQLite file readable, the SQLite-ro path is taken without errors.
    - Negative Tests:
      - With Zotero closed AND the SQLite file unreadable (file lock, permission, missing), the UI shows an "offline — using cached snapshot from <timestamp>" indicator and still renders the last-known items.
      - The plugin never writes to `zotero.sqlite`.
  - AC-5.2: arXiv metadata is fetched via `arxiv-rs` and merged into the Papers list with deduplication by DOI/arXiv ID against Zotero items.
    - Positive Tests:
      - An arXiv item already in Zotero appears once with an "in-library" badge, not as two cards.
    - Negative Tests:
      - Items that share a substring of title/author but have distinct DOIs are NOT collapsed.
  - AC-5.3: arXiv query/recommendation source is rule-based for MVP (categories + Zotero tag affinity); personalized ML-style ranking is out of MVP scope.
    - Positive Tests:
      - Configuring an arXiv category subscription produces fresh items in the tab.
    - Negative Tests:
      - Changing the rule does not require restarting the app.
  - AC-5.4: arXiv API access is polite: results are cached and refresh cadence has a minimum interval (target **~30 minutes per query** [DIRECTIONAL, confirmed by user — same-order-of-magnitude is acceptable]); manual refresh respects a short rate guard.
    - Positive Tests:
      - Rapid clicks on "refresh" do not exceed the rate guard.
    - Negative Tests:
      - The app does not poll arXiv with effective per-query frequency higher than the configured minimum (target ~30 min) by more than one order of magnitude.

- **AC-6: Application shell handles cold-start and corruption paths.**
  - AC-6.1: Three MVP tabs render on first launch with empty/placeholder state; no single plugin failure prevents the others from rendering.
    - Positive Tests:
      - First launch (no DB, no tokens) shows three tabs, each with its own onboarding/empty state.
    - Negative Tests:
      - Gmail plugin failing to initialize does not prevent Terminal Mesh and Papers from rendering.
  - AC-6.2: SQLite corruption or missing files trigger plugin-scoped recovery UI; the host does not crash.
    - Positive Tests:
      - Deleting a single plugin's DB and relaunching shows that plugin's "fresh state" onboarding.
    - Negative Tests:
      - Corrupting one plugin's DB does not break other plugins.

## Path Boundaries

> **Convention used here**: The Lower Bound describes the **minimum implementation that still passes ALL acceptance criteria**. The Upper Bound describes the **most polished implementation that does not over-engineer**. The slack between them lives in UI polish, extra UX surfaces, and platform reach — NOT in AC-required behaviors.

### Upper Bound (Maximum Acceptable Scope)
- **Plugin contract**: full lifecycle hooks (mount / unmount / error / loading / empty), per-mount `PluginCapability` token rotation with opaque non-serializable handle, generated Rust dispatcher + TS wrappers + frontend tab registry, refined error surfaces, optional per-plugin telemetry hooks.
- **Terminal Mesh**: ≥4 concurrent PTYs with refined backpressure (coalesce-on-overflow), 1 MB ring buffer per PTY, agent-marker OSC support, prompt-waiting heuristic for shells where it is reliable, polished completion animations.
- **Gmail**: multi-account read + modify (label / archive / mark-read / mark-unread / trash). Permanent delete is NOT included (would require `gmail.delete` scope). Incremental sync with rate-limit backoff curve, attachment list-and-open-on-demand, polished re-auth UX.
- **Papers**: full three-mode Zotero source (Live / SQLite-ro / cached snapshot), arXiv with rule-based recommendations, polite caching, dedup with in-library badge, snapshot-timestamp indicator.
- **Application shell**: Chinese UI strings throughout (DEC-11 resolved: Chinese-first; English deferred to Phase 2 with an i18n key layer ready for later translation), shared design system across plugins, plugin-scoped recovery UIs, error boundaries at every plugin boundary.
- **Packaging**: macOS code-signing / notarization is **stretch**, not required for local MVP validation.

### Lower Bound (Minimum Acceptable Scope — still passes ALL ACs)
- **Plugin contract**: manifest enforced, generated typed IPC wrappers, generated Rust registry AND frontend tab registry, per-plugin DB files, mandatory permission gating, non-spoofable caller identity via `PluginCapability`, lifecycle hooks including error boundary. (All AC-1 sub-criteria are required.)
- **Terminal Mesh**: ≥4 concurrent PTYs (required by AC-2.1, HARD), stdin/resize/exit/cwd/env support, host-managed lifetime with clean teardown, cancellation + 1 MB ring buffer (HARD). UI may be minimal (no animations, basic tab switcher).
- **Notifications**: BOTH native + tray on completion (required by AC-3.1), dedup window, basic permission-denied banner. UI may be unstyled.
- **Gmail**: ≥2 accounts (required by AC-4.1, HARD), incremental sync with `historyId` and 429 backoff (required by AC-4.2), Stronghold-only tokens (required by AC-4.3). Inbox UI may be minimal — table view with subject/sender/date.
- **Papers**: all three Zotero access modes (required by AC-5.1), DOI/arXiv-ID dedup (required by AC-5.2), at least one rule source (required by AC-5.3), polite caching guard (required by AC-5.4 — same-order-of-magnitude OK). Recommendation UI may be a simple list.
- **Application shell**: three tabs render with empty states (required by AC-6.1), plugin-scoped corruption recovery (required by AC-6.2), Chinese UI strings (DEC-11). i18n key layer may be deferred to Upper Bound.

### Allowed Choices
- **Can use**: Tauri v2; React 18+ / TypeScript; Rust (stable, 2024 edition); `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold` (primary secret store), `rusqlite`; for Gmail: `google-gmail1` (Google REST, preferred); for arXiv: `arxiv-rs`; for Zotero: local HTTP API + SQLite read-only; for IPC type-codegen: `ts-rs` or `specta` or equivalent.
- **Cannot use**: Electron; pure-web SPA without desktop shell; SwiftUI / AppKit-native; tmux-as-system-of-record; notebook-cell UI paradigm; MCP-server-only architecture; raw OS keychain APIs as primary token store (Stronghold is primary; keychain may be a Phase-2 alternative); raw `invoke('...')` strings in plugin code; hand-maintained plugin enumeration in the router; encrypted SQLite (DEC-10 resolved: plaintext SQLite + Stronghold for secrets is the agreed posture).

## Feasibility Hints and Suggestions

> **Note**: This section is for reference and understanding only. These are conceptual suggestions, not prescriptive requirements.

### Conceptual Approach

1. **Host skeleton** (Tauri v2 scaffold + React/TS shell + tab router consuming `src/generated/plugin-tabs.ts`) is built first. The shell exposes the `PluginHost` Rust trait and a `PluginTab` TS interface.
2. **Plugin contract** is formalized as: `plugin.toml` manifest schema + Rust traits + a `build.rs` script that scans `plugins/*/plugin.toml` and emits:
   - `src-tauri/src/generated_plugin_registry.rs` — Rust registry + dispatcher.
   - `src/generated/plugins/<id>.ts` — typed IPC wrappers per plugin.
   - `src/generated/plugin-tabs.ts` — frontend tab registry with lazy imports.
3. **Typed IPC code-gen**: use `ts-rs` (or `specta`) on Rust command/event/error types; the build script emits TS files. The generated Rust dispatcher routes IPC calls to the right plugin and enforces permission gating against the call's `PluginCapability`.
4. **Caller-identity model (`PluginCapability`)**: at plugin mount, the host generates a session token bound to `(plugin_id, mount_id)`. The capability is opaque (not JSON-serializable, not exposed on `window`, held only in the plugin component's closure/context). The host hands the plugin frontend the `PluginCapability` via the lifecycle hook. Generated wrappers require the capability as a hidden first parameter. The Rust dispatcher validates the token against its live mount registry and rotates tokens on unmount/remount. Plugin B cannot call plugin A's gated commands because B holds only B's capability.
5. **Threat model note**: For MVP, all plugin frontends run inside a single Tauri WebView. Plugin-vs-plugin isolation rests on (a) capability not being globally exposed and (b) Rust-side dispatcher binding by `(plugin_id, mount_id)`. Plugin-vs-malicious-content isolation (e.g., HTML loaded from a Gmail body) requires the plugin to render untrusted content inside a sandboxed sub-frame. Per-plugin WebViews are a Phase-2 hardening option if the threat model tightens.
6. **Per-plugin SQLite**: each plugin owns `${APP_DATA}/plugins/<id>/state.sqlite`. The host hands the plugin a `PluginDb` handle that opens only that file. Migrations are owned by the plugin via `refinery` or `sqlx::migrate!`. Plaintext SQLite is the agreed posture (DEC-10); secrets live in Stronghold only.
7. **Stronghold**: store OAuth refresh tokens keyed by `(plugin_id, account_id)`. Access tokens stay in process memory only.
8. **Terminal Mesh** wraps `portable-pty` in a Tokio-actor (one task per PTY) emitting `TerminalEvent::Output | Resize | Exit | NeedsAttention`. xterm.js subscribes via the typed event channel. Output buffering is a 1 MB ring per PTY with coalesce-on-overflow.
9. **Notification surface** is a host-owned service subscribing to plugin events with the `notify` capability; it applies a host-side dedup window (~2 s per `(plugin_id, event_id)`) before firing native + tray.
10. **Gmail plugin** drives OAuth via the system browser (Tauri URL handler), stores refresh tokens in Stronghold, syncs incrementally via `historyId` with a fallback full-sync on `historyNotFound (404)` and exponential backoff on `429`.
11. **Papers plugin** probes Zotero HTTP API at `localhost:23119` on startup; falls back to SQLite read-only on `zotero.sqlite`; the plugin maintains its own materialized snapshot for the third-mode offline path.

### Specification Deliverables (produced by `analyze`-tagged tasks)
- `docs/specs/plugin-contract.md` — manifest schema, IPC type-gen pipeline, permission enumeration, lifecycle hooks, `PluginCapability` design.
- `docs/specs/terminal-events.md` — needs-attention taxonomy (completion / nonzero exit / prompt-waiting / OSC agent marker; explicitly NOT stderr-burst). Defines event payload schema and dedup keys. Written BEFORE notification implementation.
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
   - Phase G: Frontend plugin lifecycle hooks, `PluginCapability` issuance/rotation, and error boundary.

2. **Terminal Mesh** — first plugin; stress-tests the contract; unblocks the notification surface.
   - Phase A: Terminal event taxonomy spec at `docs/specs/terminal-events.md` (written BEFORE notification implementation).
   - Phase B: portable-pty Tokio actor with 1 MB ring buffer per PTY and cancellation.
   - Phase C: xterm.js frontend with multi-tab terminal UI + state-store wiring + concurrent-PTY stress test (≥4 PTYs).
   - Phase D: Notification surface (native + tray window) with host-side dedup/throttle keyed by `(plugin_id, terminal_id, event_kind)`.
   - Phase E: Permission-denial graceful fallback.

3. **Gmail Plugin** — depends on Stronghold and typed IPC.
   - Phase A: OAuth flow per account + Stronghold token storage.
   - Phase B: Gmail sync edge-case spec at `docs/specs/gmail-sync.md`.
   - Phase C: Account-keyed sync DB + historyId-based incremental sync + rate-limit backoff.
   - Phase D: Inbox UI + account switcher + revoke/re-auth UX + attachment list/open-on-demand.

4. **Papers Plugin (Zotero + arXiv)** — depends on the SQLite framework.
   - Phase A: Zotero local HTTP API probe + client (Live mode).
   - Phase B: SQLite read-only fallback (`zotero.sqlite` with `?mode=ro&immutable=1`).
   - Phase C: App-owned cached snapshot (third-mode offline path).
   - Phase D: arxiv-rs query engine + rule-based recommendation logic + polite caching.
   - Phase E: Merged Papers list UI with dedup + offline indicator.

Relative dependencies (not time-based):
- Milestone 1 must be largely complete before Milestone 2 starts (the contract is the substrate).
- Milestone 2 (Terminal Mesh) stress-tests the contract; small Milestone-1 revisions are expected during Milestone 2.
- Milestones 3 and 4 can proceed in parallel once Milestone 1 stabilizes and Milestone 2 has shaken out the IPC/DB/Stronghold patterns.

## Task Breakdown

Each task includes exactly one routing tag:
- `coding`: implemented by Claude (Anthropic's coding model)
- `analyze`: produced by Codex via `/humanize:ask-codex` (specification artifacts)

| Task ID | Description | Target AC | Tag | Depends On |
|---------|-------------|-----------|-----|------------|
| task1 | Scaffold Tauri v2 + React/TS shell with empty 3-tab UI router consuming the generated frontend registry | AC-6.1, AC-1.7 | coding | - |
| task2 | Write plugin contract spec at `docs/specs/plugin-contract.md` (manifest schema, IPC pipeline, permission enumeration, lifecycle hooks, `PluginCapability` design) | AC-1.1, AC-1.2, AC-1.4, AC-1.5, AC-1.6 | analyze | task1 |
| task3 | Implement `build.rs` plugin scanner that emits Rust registry + per-plugin TS wrappers + frontend tab registry (`src/generated/plugin-tabs.ts`) | AC-1.1, AC-1.7 | coding | task2 |
| task4 | Implement typed IPC code-gen pipeline (Rust SoT → TS wrappers via ts-rs or specta) consumed by generated frontend imports | AC-1.2 | coding | task2 |
| task5 | Implement the generated Rust IPC dispatcher with permission gating bound to `PluginCapability` (opaque, non-serializable, rotating on unmount) | AC-1.4, AC-1.6 | coding | task3, task4 |
| task6 | Implement per-plugin SQLite file layout (`${APP_DATA}/plugins/<id>/state.sqlite`) and migration framework (plaintext per DEC-10) | AC-1.3 | coding | task1 |
| task7 | Integrate tauri-plugin-stronghold for OAuth refresh-token storage keyed by `(plugin_id, account_id)` | AC-4.3 | coding | task1 |
| task8 | Implement frontend plugin lifecycle hooks (`onMount` issuing `PluginCapability`, `onUnmount` rotating it, `onError`) and host-level error boundary | AC-1.5, AC-1.6, AC-6.1 | coding | task5 |
| task9 | Write terminal event taxonomy spec at `docs/specs/terminal-events.md` (completion / nonzero exit / prompt-waiting / OSC agent marker; explicitly NOT stderr-burst) | AC-3.2 | analyze | task1 |
| task10 | Wrap portable-pty in a Tokio actor with 1 MB ring buffer per PTY, cancellation, and `TerminalEvent` stream per the taxonomy spec | AC-2.2, AC-2.4 | coding | task4, task9 |
| task11 | Build xterm.js multi-tab frontend with PTY backend wiring; concurrent-PTY stress test (≥4 PTYs simultaneous) | AC-2.1, AC-2.3 | coding | task10 |
| task12 | Implement notification surface: native banner + menubar tray window with host-side dedup/throttle keyed by `(plugin_id, terminal_id, event_kind)` | AC-3.1, AC-3.2 | coding | task9, task11 |
| task13 | Handle macOS notification permission denial with in-app fallback banner | AC-3.3 | coding | task12 |
| task14 | Implement Gmail OAuth flow (system browser) with per-account Stronghold storage | AC-4.1 | coding | task7, task5 |
| task15 | Write Gmail sync edge-case spec at `docs/specs/gmail-sync.md` (historyNotFound recovery, 429 backoff curve, watch-vs-poll tradeoff, attachment listing) | AC-4.2 | analyze | task14 |
| task16 | Implement Gmail sync DB + incremental sync engine + rate-limit backoff per the spec | AC-4.2 | coding | task15, task6 |
| task17 | Build Gmail inbox UI (Chinese strings per DEC-11) + account switcher + re-auth UX + attachment list/open-on-demand | AC-4.1 | coding | task16 |
| task18 | Implement Zotero local HTTP API client (probe `localhost:23119`) with SQLite read-only fallback and app-owned cached snapshot | AC-5.1 | coding | task6 |
| task19 | Implement arxiv-rs query engine + rule-based recommendation + polite caching (target ~30 min minimum refresh interval) | AC-5.2, AC-5.3, AC-5.4 | coding | task18 |
| task20 | Build Papers tab UI (Chinese strings per DEC-11) with merge/dedup against Zotero + offline indicator (timestamp of cached snapshot) | AC-5.1, AC-5.2, AC-5.3 | coding | task19 |
| task21 | Implement plugin-scoped SQLite corruption recovery + per-plugin reset UX | AC-6.2 | coding | task6 |

## Claude-Codex Deliberation

### Agreements
- Plugin contract is formalized as: manifest + typed IPC + per-plugin DB files + mandatory permission gating + lifecycle hooks + non-spoofable caller identity (`PluginCapability`) + generated frontend tab registry. All AC-1 sub-criteria are required.
- PTY lifecycle: one Tokio actor per PTY with 1 MB ring buffer and cancellation.
- Gmail multi-account requires consistent `account_id` keying across Stronghold, sync DB, and UI.
- Zotero three-mode access: Live (HTTP `localhost:23119`) → SQLite read-only (`?mode=ro&immutable=1`) → app-owned cached snapshot.
- Notification dedup/throttle is host-owned, keyed by `(plugin_id, terminal_id, event_kind)`.
- OAuth refresh tokens live in Stronghold only; SQLite is plaintext but holds only non-secret metadata (DEC-10 resolved).
- Compile-time plugin registry via `build.rs` scanning `plugins/`; user-installable plugins are Phase 2.
- IPC schema generated from a Rust source-of-truth; raw `invoke('...')` strings are forbidden in plugin code.
- SQLite isolation via separate DB files per plugin; no cross-plugin SQL.
- Permission gating is mandatory in the lower bound; warning-only is not acceptable.
- Gmail Upper Bound is read + modify (label / archive / mark-read / trash); permanent delete (`gmail.delete`) deferred to Phase 2 — consistent with DEC-3 / DEC-4.
- Stderr-burst is NOT a default needs-attention trigger.
- `analyze`-tagged tasks deliver named spec documents under `docs/specs/`.
- Caller identity enforced via per-mount `PluginCapability` (opaque, non-serializable, rotating); generated wrappers require it as a hidden parameter; Rust dispatcher validates against live mount registry by `(plugin_id, mount_id)`.
- Threat model: single WebView for MVP; per-plugin WebViews are Phase 2 hardening.
- Quantitative thresholds: ≥4 PTYs (AC-2.1), ≥2 Gmail accounts (AC-4.1), 1 MB PTY ring buffer (AC-2.4) are HARD; ~30 min arXiv refresh (AC-5.4) is DIRECTIONAL (user-confirmed).

### Resolved Disagreements (across 3 convergence rounds)
- **IPC schema generation** (round 1): typed wrappers generated from Rust SoT (ts-rs/specta) instead of raw `invoke('...')` strings.
- **SQLite isolation** (round 1): per-plugin DB files instead of shared connection with table prefixes.
- **Permission gating posture** (round 1): mandatory in lower bound (not warning-only).
- **Zotero offline modes** (round 1): three distinct modes — Live / SQLite-ro / cached snapshot.
- **Gmail Upper Bound vs DEC-3** (round 1): read + modify only, no send/reply.
- **Needs-attention taxonomy** (round 1): stderr-burst removed from defaults.
- **Stronghold vs OS keychain** (round 1): Stronghold primary; OS keychain is Phase-2 alternative.
- **AC vs Lower Bound alignment** (round 2): Lower Bound now satisfies every AC; slack lives in UI polish, not in AC-required behaviors.
- **Generated frontend tab registry** (round 2): added as AC-1.7 and explicit task output.
- **Caller identity** (round 2): per-mount `PluginCapability` enforced at Rust dispatcher; opaque/non-serializable wording added (round 3 polish).
- **Gmail delete scope** (round 2): `gmail.modify` covers trash; permanent delete (`gmail.delete`) deferred.

### Convergence Status
- Final Status: **`converged`**
- Rounds executed: 3 (Codex round-3 explicitly stated "Convergence is reached. Remaining items are user decisions, not plan blockers.")

## Pending User Decisions

Decisions originally raised as open questions by Codex's first-pass analysis. Items marked `RESOLVED` were ratified by the user during Phase 6 review; items marked `PENDING` carry Codex's agreement with Claude's tentative answer but await explicit user ratification before implementation. The `start-rlcr-loop` step is the recommended place to ratify or revise PENDING items.

- **DEC-1: Terminal session persistence across app restart.**
  - Claude Position: out of MVP scope.
  - Codex Position: agrees (default out of MVP).
  - Tradeoff Summary: persistence requires either a session daemon (tmux-style) or PTY-state serialization; both add packaging complexity.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-2: Exact terminal events that trigger 灵动岛-style alert.**
  - Claude Position: child exit (zero), child exit (nonzero), prompt-waiting heuristic (if reliably detectable), explicit OSC agent marker. NOT stderr-burst.
  - Codex Position: agrees with this default set.
  - Tradeoff Summary: too few triggers → users miss state; too many → notification fatigue.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-3: Gmail MVP scope — read-only vs read+modify vs read+send.**
  - Claude Position: read + modify (label / archive / mark-read / trash) in MVP; send/reply deferred to Phase 2.
  - Codex Position: agrees with deferring send/reply.
  - Tradeoff Summary: send/reply adds compose UX and draft management which is non-trivial.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-4: Gmail OAuth scopes acceptable.**
  - Claude Position: `gmail.modify` (read + add/remove labels + archive + mark-read/unread + trash). NOT `gmail.delete` (permanent delete deferred). NOT `gmail.send`.
  - Codex Position: agrees, tied to DEC-3.
  - Tradeoff Summary: `gmail.modify` is the minimum scope that covers triage; broader scopes increase blast radius if tokens leak.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-5: Attachment handling in Gmail.**
  - Claude Position: list attachments inline with the message; open-on-demand via the system default app; no auto-download or indexing in MVP.
  - Codex Position: agrees.
  - Tradeoff Summary: indexing costs DB space and processing; on-demand keeps MVP simple.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-6: Does Papers MVP require the Zotero desktop app to be running?**
  - Claude Position: no — three-mode access (Live / SQLite-ro / cached snapshot) ensures graceful offline.
  - Codex Position: agrees with three distinct modes.
  - Tradeoff Summary: three modes add code; single mode forces user to keep Zotero running.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-7: arXiv recommendations — rule-based vs personalized ranking.**
  - Claude Position: rule-based for MVP (categories + Zotero tag affinity); personalized ranking deferred.
  - Codex Position: agrees.
  - Tradeoff Summary: personalized ranking needs an embedding pipeline.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-8: Cross-plugin global search (e.g., search across Gmail + Zotero + terminal scrollback).**
  - Claude Position: out of MVP scope.
  - Codex Position: agrees.
  - Tradeoff Summary: cross-plugin index is a non-trivial schema decision; defer until plugin contract has matured.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-9: Plugin install model — build-time vs user-installable.**
  - Claude Position: build-time only for MVP (via `plugins/<id>/` + `build.rs` scan); user-installable is Phase 2 with sandboxing.
  - Codex Position: agrees.
  - Tradeoff Summary: user-installable plugins require capability sandboxing and signing.
  - Decision Status: PENDING (Codex agrees with Claude; user has not explicitly ratified).

- **DEC-10: Local-DB encryption posture.**
  - Claude Position: plaintext SQLite; secrets only in Stronghold.
  - Codex Position: agrees with default; flagged for user confirmation.
  - User Decision: **RESOLVED — plaintext SQLite + Stronghold for secrets** (user confirmed during Phase 6 review).
  - Decision Status: RESOLVED.

- **DEC-11: I18n posture.**
  - Claude Position (initial): CN/EN bilingual from day one.
  - Codex Position: bilingual acceptable but not required for MVP.
  - User Decision: **RESOLVED — MVP is Chinese-first; English deferred to Phase 2.** (User confirmed during Phase 6 review.)
  - Decision Status: RESOLVED.

- **DEC-12: Data retention policy.**
  - Claude Position: terminal scrollback in-memory only; Gmail cached metadata kept until plugin reset; attachments not downloaded (open-on-demand only).
  - Codex Position: surfaced as open question.
  - User Decision: **RESOLVED — defaults accepted** (user confirmed during Phase 6 review).
  - Decision Status: RESOLVED.

## Implementation Notes

### Code Style Requirements
- Implementation code and comments must NOT contain plan-specific terminology such as "AC-", "Milestone", "Step", "Phase", or similar workflow markers.
- These terms are for plan documentation only, not for the resulting codebase.
- Use descriptive, domain-appropriate naming in code instead (e.g., the host trait is `PluginHost`, not `MilestoneOnePluginHost`).

### Threat Model Assumptions
- MVP runs all plugin frontends inside a single Tauri WebView.
- Plugin-vs-plugin isolation relies on (a) `PluginCapability` not being globally exposed (held in component closure) and (b) Rust-side dispatcher binding by `(plugin_id, mount_id)`.
- Plugin-vs-malicious-content isolation (e.g., HTML rendered from a Gmail body) requires the plugin to render untrusted content inside a sandboxed sub-frame; this is a plugin-author responsibility, not a host guarantee.
- Per-plugin WebViews and a stricter sandbox are Phase-2 hardening targets if the threat model tightens.

--- Original Design Draft Start ---

# Tauri Plugin-Hosted Personal Agent And Information Workspace

## Original Idea

我希望你在这个工作区里面开一个新仓库，这个仓库将要作为一个agent平台，担任我所有信息管理（邮件收发（连接gmail多个账号）、消息管理（连接微信）、论文管理和推荐（连接我的zotero和arxiv））、agent终端管理（我会开多个终端，我需要能实时查看各个终端的状态，并且某个终端工作完成了或者需要我操作了给我一个灵动岛提示）；并且需要支持增加更多的tab功能（比如后期我可能需要github仓库追踪和辅助pr code review功能等）；你要考虑到工程可维护性、前端美观度以及可扩展性

## Primary Direction: Tauri Plugin-Registry Workspace

### Rationale

A Tauri (Rust + web frontend) desktop app with a VS Code-style tab/panel system; each integration (Gmail, WeChat, Zotero, arXiv, terminal) is a frontend plugin paired with a Rust-side adapter against a stable host API — distinguished from native and pure-web approaches by combining native-OS performance with web-tech UI velocity and a first-class plugin contract.

### Approach Summary

A **Tauri v2 desktop application** paired with a modular plugin architecture serving as the unified information and agent-terminal platform. The design follows the established patterns of VS Code (container/item abstraction, extension host isolation) and Obsidian (lifecycle-aware plugin API, declarative registration).

**Core architecture:**
- **Frontend (React + TypeScript):** Tab/panel system with drag-and-drop UI inspired by VS Code, hosting plugin-provided webview components. A shared design system enforces visual coherence across plugins, addressing the "前端美观度" requirement directly.
- **Tauri host (Rust):** Stable IPC command/event API (`#[tauri::command]` macro + `tauri::emit`/`tauri::listen`) that plugins implement against; lifecycle-aware plugin loading via a standardized manifest.
- **Plugin system:** Each integration is a self-contained pair — frontend component + Rust adapter — invoking Rust commands via `tauri::invoke()` and listening for events via `tauri::listen()`. Plugins declare permissions and required APIs in a manifest:
  ```json
  {
    "name": "gmail",
    "type": "tab",
    "command": "tauri-plugin-gmail",
    "frontend": "@tauri-plugin-gmail-api",
    "permissions": ["user.email", "oauth2"],
    "requiredApis": ["invoke", "listen"]
  }
  ```
- **Sidecars:** Long-running services (PTY manager, email sync daemons, optional MCP-server bridges) spawn as Rust binaries via `ShellExt::sidecar()`, communicating back through named channels and stdout event streams. This is the seam where existing community MCP servers (Gmail-MCP, Zotero-MCP, GitHub-MCP) can be adopted later without re-architecting the host.
- **Terminal management:** xterm.js frontend + portable-pty Rust backend, with each terminal as a tab or sub-panel. Process state (PID, exit code, last activity) streams via `tauri::emit()` and is mirrored to the notification layer.
- **Notifications ("灵动岛提示"):** `tauri-plugin-notification` for native macOS UNUserNotificationCenter alerts; `tauri-plugin-positioner` for a menubar tray window pinned `TrayBottomCenter`, acting as a desktop analog of Dynamic Island for terminal-completion and "needs your attention" events.
- **State:** SQLite (rusqlite) for per-plugin persistent state keyed by plugin ID; OAuth tokens stored in OS keychain via tauri-plugin-stronghold.

**Initial tab set (MVP):** Gmail (multi-account), Zotero+arXiv (papers + recommendations), Terminal Mesh, Inbox/notifications digest. Phase 2 adds WeChat bridge (subject to feasibility review) and GitHub PR review.

### Objective Evidence

- exploratory greenfield — repo at `/Users/rockdu/claude_workspace` is empty, so evidence is grounded in adjacent open-source tooling rather than internal precedent
- [Tauri v2 Architecture](https://v2.tauri.app/concept/architecture/) — window APIs, lifecycle hooks, IPC primitives
- [Tauri v2 Plugin Development](https://v2.tauri.app/develop/plugins/) — plugin manifest schema, Cargo crate structure, JS/Rust binding pattern
- [tauri-apps/plugins-workspace](https://github.com/tauri-apps/plugins-workspace) — 20+ maintained official plugins (notification, shell, fs, dialog, positioner) demonstrating the contract and build tooling
- [Tauri v2 IPC](https://v2.tauri.app/concept/inter-process-communication/) — `invoke` (RPC-style Promise) and `listen` (event subscription) as dual primitives
- [Tauri Sidecar guide](https://v2.tauri.app/develop/sidecar/) — compile external binaries into the app and stream stdout; cross-platform target-triple naming
- **Direct precedent — Terax AI**: [crynta/terax-ai](https://github.com/crynta/terax-ai) — Tauri v2 + React + xterm.js + portable-pty multi-tab terminal emulator (~7 MB binary); proves the terminal-mesh portion of this design ships today
- [xterm.js](https://github.com/xtermjs/xterm.js/) — mature web terminal emulator (powers VS Code's integrated terminal, GitHub Codespaces)
- [portable-pty crate](https://docs.rs/portable-pty) — cross-platform PTY abstraction (ConPTY on Windows, native pty on macOS/Linux); [tauri-plugin-pty](https://lib.rs/crates/tauri-plugin-pty) wraps it for Tauri
- [google-gmail1 crate](https://docs.rs/google-gmail1) — OAuth2-aware Gmail API bindings, generated from Google's official schema; [rust-imap](https://github.com/jonhoo/rust-imap) is the IMAP fallback for non-Gmail accounts
- [Zotero direct SQLite access](https://www.zotero.org/support/dev/client_coding/direct_sqlite_database_access) and [Zotero Web API v3](https://www.zotero.org/support/dev/web_api/v3/basics) — two officially supported paths; [Zotero 7+ local HTTP API](https://forums.zotero.org/discussion/97980/) is the preferred path going forward
- [arxiv-rs crate](https://docs.rs/arxiv-rs) — async Rust wrapper over the arXiv XML API with query builder, pagination, PDF download
- WeChat options: [Wechaty](https://github.com/wechaty/wechaty) (reverse-engineered protocols), ntchat, WeChatPadPro — all carry platform and legal risk; alternative [WeChat-MCP (BiboyQG)](https://github.com/BiboyQG/WeChat-MCP) uses macOS accessibility API automation
- [tauri-plugin-notification](https://github.com/tauri-apps/tauri-plugin-notification) — native macOS UNUserNotificationCenter; [tauri-plugin-positioner](https://github.com/tauri-apps/plugins-workspace/tree/main/plugins/positioner) supports menubar window pinning (`TrayBottomCenter`) — the closest desktop analog to Dynamic Island
- Plugin host design references: [VS Code Extension API architecture](https://code.visualstudio.com/api), [Obsidian Plugin API](https://github.com/obsidianmd/obsidian-api) (Component lifecycle hierarchy with automatic cleanup), [Logseq plugin docs](https://plugins-doc.logseq.com/) (sandboxed registration)

### Known Risks

- **WeChat bridge feasibility:** Official WeChat API is restricted; Wechaty / ntchat / WeChatPadPro rely on reverse-engineered protocols and break when Tencent ships updates. Treat as Phase-2; do a feasibility + compliance review before committing.
- **Multi-account Gmail complexity:** `google-gmail1` requires a separate OAuth2 token per account with no built-in account-switching. Mitigate by keying plugin state on `account_id` and centralizing token refresh in the plugin's adapter layer.
- **Zotero access contention:** Direct SQLite is safe but tied to the local Zotero install; Zotero 7+ local HTTP API is forward-compatible but version-gated. Provide both paths and document the Zotero version requirement.
- **Plugin isolation limits:** Tauri plugins share the same Rust runtime; a panicking plugin can destabilize the host (no per-extension process isolation like VS Code). Mitigate via panic hooks at plugin init, a "stable / experimental" tier in the manifest, and enforced API contracts.
- **Terminal state sync latency:** PTY → UI must feel live (<500 ms). Naive polling burns CPU; use `tauri::emit()` on state changes plus 16 ms batched output flushes.
- **"灵动岛" fidelity gap:** macOS has no real Dynamic Island. The menubar-tray-window pattern is the best practical analog and matches what users of similar tools (e.g., Hotlist) expect, but it is not pixel-equivalent to iOS Live Activities — if true Dynamic Island fidelity is non-negotiable, see Alt-3.

## Alternative Directions Considered

### Alt-1: MCP-Federated Control Room
- Gist: A thin orchestrator/dashboard where every integration (Gmail, WeChat, Zotero, arXiv, terminal, GitHub) is a standalone Model Context Protocol server, and tabs auto-derive from connected servers via `list_tools` / `list_resources` discovery. Adding a new tab means publishing a new MCP server — the core never changes.
- Objective Evidence:
  - [MCP Specification 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) — stdio, SSE, Streamable HTTP transports with Tools / Resources / Prompts primitives
  - Shipping community servers for every required source: [Gmail-MCP-Server](https://github.com/GongRzhe/Gmail-MCP-Server), [Zotero-MCP](https://github.com/54yyyu/zotero-mcp), [ZotLink](https://github.com/TonybotNi/ZotLink) (arXiv-aware), [mcp-shell-server](https://github.com/mako10k/mcp-shell-server), [pm-mcp](https://github.com/patrickjm/pm-mcp), [WeChat-MCP](https://github.com/BiboyQG/WeChat-MCP), [official github-mcp-server](https://github.com/github/github-mcp-server)
  - Direct precedent: [MCP Dashboard (triepod-ai)](https://github.com/triepod-ai/mcp-dashboard) — React/Tailwind multi-server dashboard with discovery + execution UI
  - [MCP Tasks abstraction (Nov 2025)](https://blog.modelcontextprotocol.io/posts/2025-11-25-first-mcp-anniversary/) standardizes long-running-task tracking — fits terminal completion notifications natively
- Why not primary: Auto-derived UIs from server manifests fragment the visual experience, working against the user's stated "前端美观度" goal; the primary design captures Alt-1's federation benefit by allowing MCP servers as plugin sidecars without giving up bespoke UI control.

### Alt-2: Unified Event-Stream Inbox
- Gist: Center the platform on a single timestamped event store (SQLite + sqlite-vec, or DuckDB) into which every source ingests; the primary UI is a triage timeline with tabs as filtered projections over the event log. Terminal status changes are just another event type. Cross-source correlation ("show me all unread items about topic X") becomes a single query.
- Objective Evidence:
  - [Beeper universal inbox](https://www.beeper.com/) and [Slack Unified Grid](https://slack.engineering/unified-grid-how-we-re-architected-slack-for-our-largest-customers/) validate multi-source aggregation at production scale
  - [Event Sourcing](https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing) + [Materialized View](https://learn.microsoft.com/en-us/azure/architecture/patterns/materialized-view) patterns (CQRS) — proven approach to per-tab projections over a shared log
  - [sqlite-vec](https://github.com/asg017/sqlite-vec) for SIMD-accelerated vector search on the event payloads; SQLite handles ~30–40k writes/sec, comfortable for personal-scale ingestion
  - [Claude Island (farouqaldori)](https://github.com/farouqaldori/claude-island) — recent macOS menubar precedent for streaming task state into a Dynamic-Island-like surface
- Why not primary: Largest core implementation surface (custom schema, ingestion pipelines, materialized views, embeddings) and inverts the user's mental model — they asked for tabs and an agent platform, not an inbox-first product; can be folded back into the primary as the persistence layer underneath the plugins.

### Alt-3: Native SwiftUI + Live Activities
- Gist: A first-class macOS SwiftUI app with an iOS companion that delivers *real* Dynamic Island Live Activities via ActivityKit. Integrations as Swift packages; terminal hosting via Foundation.Process + PTY with SwiftTerm for VT100 rendering; mac↔iOS communication via App Groups + Darwin notifications.
- Objective Evidence:
  - [Apple ActivityKit / Live Activities HIG](https://developer.apple.com/design/human-interface-guidelines/live-activities) — 4 KB payload limit, 8-hour active window, APNs push updates; iOS-only (no Mac Dynamic Island)
  - [SwiftUI multi-window](https://developer.apple.com/documentation/swiftui/bringing-multiple-windows-to-your-swiftui-app) + `NavigationSplitView` for the workspace shell
  - Open-source references: [Pepitta](https://github.com/ankorez/pepitta) (SwiftUI Gmail client), [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm) (terminal emulator), [macOS Tahoe](https://support.apple.com/en-us/122868) mirrors iPhone Live Activities into the Mac menu bar
  - No native Swift package for Zotero; arXiv integration would be hand-rolled REST
- Why not primary: macOS/iOS-only with a much larger LOC surface (REST clients re-implemented in Swift, no Python/Node ecosystem reuse), and the "real Dynamic Island" payoff only lands if the user actually carries the iOS companion — high cost for a single UX detail.

### Alt-4: tmux/Zellij-Backed Terminal Mesh
- Gist: Terminal multiplexer is the system of record for sessions. The web dashboard attaches via libtmux or tmux control mode (`-CC`); information sources are delivered either as tmux panes (CLI subscribers piping into a pane) or as dashboard widgets adjacent to the terminal grid. Shell hooks (precmd / OSC 9 / OSC 777) fire macOS notifications on long-command completion.
- Objective Evidence:
  - [tmux control mode wiki](https://github.com/tmux/tmux/wiki/Control-Mode), [libtmux](https://github.com/tmux-python/libtmux), [zellij plugin system](https://zellij.dev/documentation/plugins.html) — mature, decades-proven multiplexer foundations
  - [Claude Code multi-agent tmux setup](https://medium.com/medialesson/claude-code-multi-agent-tmux-setup-7361b71ff5c4) — official documented pattern for orchestrating parallel agents in tmux panes
  - Real-world dashboards: [agent-tmux-manager](https://github.com/damelLP/agent-tmux-manager), [tmuxwatch](https://github.com/steipete/tmuxwatch); [ttyd](https://github.com/tsl0922/ttyd) / [gotty](https://github.com/yudai/gotty) for browser pane streaming
  - [OSC 9 / OSC 777 notification escape sequences](https://www.karl.berlin/terminal-notifications.html) — shell-triggered notifications without an OS-specific daemon
- Why not primary: Awkward shape for non-terminal use cases (reading email or browsing a Zotero library inside a tmux pane is unidiomatic), and shipping a polished frontend for email/papers requires the same plugin contract as the primary design — so the primary already covers terminals while leaving room for the other tabs.

### Alt-5: Notebook-Cell Workspace
- Gist: Inspired by Jupyter / Marimo / Notion — every "tab" is a notebook page whose cells can be email queries, Zotero lookups, terminal panes, arXiv feeds, or markdown notes, with reactive cross-cell linking (Observable-style dependency DAG) and shareable workspace files (`.ipynb` or pure `.py`).
- Objective Evidence:
  - [Marimo reactive notebooks](https://docs.marimo.io/guides/reactivity/) + [pure-Python persistence](https://marimo.io/blog/python-not-json) — DAG-driven re-execution and Git-friendly format
  - [Observable reactive dataflow](https://observablehq.com/@observablehq/reactive-dataflow) — named cells with implicit dependencies and cross-notebook imports
  - [JupyterTerminals](https://github.com/willkessler/jupyterterminals), [notebook_xterm](https://github.com/adamj9431/notebook_xterm) — terminal-as-cell precedent
  - [BlockSuite](https://github.com/toeverything/blocksuite) — CRDT-backed block-tree editor for collaborative document/whiteboard, suggesting a path to real-time collaboration
- Why not primary: Fundamental unresolved tension between "notebook = document" and "live agent platform" (emails arriving out-of-band don't fit a DAG cleanly), and the cell paradigm is a higher-friction match for the user's explicit "tab" mental model.

## Synthesis Notes

The primary design is deliberately positioned to absorb the strongest elements of the alternatives without committing to their full philosophies. **Alt-1 (MCP)** folds in via the sidecar seam: each plugin's Rust adapter can wrap an existing community MCP server (Gmail-MCP, Zotero-MCP, github-mcp-server) instead of re-implementing the integration from scratch, giving us much of the federation benefit while the host owns the look-and-feel. **Alt-2 (Event-Stream)** folds in as the persistence substrate: the primary's SQLite layer can adopt an event-log schema with materialized views per tab, enabling the cross-source triage timeline as an *additional* tab rather than the dominant UI. **Alt-4 (tmux)** folds in as the terminal plugin's optional backend: instead of (or in addition to) embedded portable-pty sessions, the terminal tab can attach to externally-managed tmux/zellij sessions via libtmux for users who already live in a multiplexer. **Alt-3 (SwiftUI)** is the harder swap — choosing it means losing cross-platform reach in exchange for true Dynamic Island fidelity; revisit only if a polished iOS companion becomes a hard requirement. **Alt-5 (Notebook)** is the most distant — its cell-as-unit-of-composition philosophy conflicts with the user's tab mental model — but a "scratchpad" tab built from BlockSuite blocks could be added later for users who want freeform composition alongside the structured plugin tabs.

--- Original Design Draft End ---
