# Ask Codex Input

## Question

You are doing a REASONABILITY REVIEW of Claude's candidate implementation plan v1 for a greenfield Tauri v2 personal-agent platform MVP. This is the second-pass critique (after a first-pass analysis already happened); Claude has now produced a structured plan that you must challenge constructively.

# Scope Reminder

- Repo: greenfield, `git init`ed today at `/Users/rockdu/claude_workspace`. No source code yet.
- Stack is LOCKED: Tauri v2 + Rust + React/TypeScript. Do NOT propose Electron, pure-web, SwiftUI, tmux, or notebook-cell. Those are already rejected.
- MVP tabs are LOCKED to exactly three: Terminal Mesh, Gmail multi-account, Zotero + arXiv Papers.
- WeChat and GitHub PR review are explicitly deferred to phase 2.
- The plugin contract MUST be general enough to host WeChat / GitHub PR review later without refactoring the host.

# Your Job

Read the candidate plan below. Then:
1. Identify what is GENUINELY reasonable and well-grounded.
2. Identify what you DISAGREE with (and why specifically — not just style preferences).
3. List REQUIRED CHANGES that must be made before convergence (things that would cause real problems in implementation, not just nits).
4. List OPTIONAL IMPROVEMENTS (nice-to-have refinements).
5. List UNRESOLVED items — places where Claude and you have opposite opinions that need the human user to decide.

# Output Format

Return EXACTLY these five sections in this order, with these exact headings:

## AGREE
- (specific points you accept as reasonable; cite AC numbers or task IDs where relevant)

## DISAGREE
- (specific points you consider unreasonable + concrete reason; cite the location in the plan)

## REQUIRED_CHANGES
- (must-fix items before convergence; phrase as actionable edits like "Change AC-2.3 to require..."; each item must point at a specific section of the plan)

## OPTIONAL_IMPROVEMENTS
- (non-blocking refinements; phrase the same way)

## UNRESOLVED
- (places where Claude's position and your position differ in a way that requires explicit user decision; phrase as "Topic: Claude=X, Codex=Y, user must choose")

Be specific. Vague critiques are not useful. If the plan is broadly reasonable, say so in AGREE and keep REQUIRED_CHANGES short.

---

# The Candidate Plan v1

```
# CANDIDATE PLAN v1 — Tauri Plugin-Hosted Personal Agent And Information Workspace (MVP)

(Internal artifact for Codex convergence review. Will be transformed into the final plan.md after convergence.)

## Goal Description

Build the MVP of a Tauri v2 desktop application that hosts three first-class plugin tabs — Terminal Mesh, Gmail (multi-account), Papers (Zotero + arXiv) — on top of a declarative plugin contract designed so future tabs (WeChat, GitHub PR review, ...) can be added by writing a new plugin pair (frontend component + Rust adapter + manifest entry) without modifying the host or other plugins. Long-running terminal state (completion, needs-attention) surfaces through a macOS menubar tray window plus a native notification, serving as the desktop analog of iOS Dynamic Island.

## Acceptance Criteria

- **AC-1: Plugin contract is declarative and host-agnostic.** A new plugin can be added by writing a manifest, a Rust adapter implementing the plugin trait, and a frontend component implementing the tab interface — without modifying host code or other plugins.
  - AC-1.1: Manifest schema enforced (name, type, command, frontend, permissions, requiredApis, dbNamespace).
    - Positive: a valid manifest registers the plugin and surfaces the tab on app launch.
    - Negative: a manifest missing a required field is rejected at load time with a typed error pointing to the missing field; the app does not crash.
  - AC-1.2: Typed IPC contract for commands, events, and errors (single source-of-truth schema generates Rust + TS types).
    - Positive: a TS call to `invoke('plugin.<name>.<command>', args)` is type-checked at compile time against the schema.
    - Negative: invoking a non-existent command, or passing args that fail schema validation, returns a typed error variant without panicking.
  - AC-1.3: Per-plugin SQLite namespace with isolated migration ownership.
    - Positive: plugin A's migration cannot read or write plugin B's tables; resetting plugin A's DB does not affect plugin B.
    - Negative: a plugin attempting a cross-namespace query receives a typed access error.
  - AC-1.4: Permission gating enforced by the host.
    - Positive: a plugin that declares the `user.email` permission can invoke Gmail-scoped commands.
    - Negative: a plugin without the `user.email` permission attempting a Gmail-scoped command receives a typed permission-denied error.

- **AC-2: Terminal Mesh supports multiple concurrent PTYs with full lifecycle control.**
  - AC-2.1: At least 4 concurrent PTY sessions can be opened simultaneously, each streaming output to its own xterm.js instance.
    - Positive: spawn 4 PTYs running long commands; all four show streaming output without blocking each other.
    - Negative: closing a PTY tab while a command runs cleans up the child process; orphans are not left on `ps aux`.
  - AC-2.2: PTY supports stdin, resize, exit-status reporting, and cwd/env at spawn.
    - Positive: typing into a PTY reaches the child process; resizing the xterm.js viewport propagates SIGWINCH; child exit code is visible in the tab.
    - Negative: spawning with an invalid shell path returns a typed error and renders an in-tab error state without crashing the app.
  - AC-2.3: Terminal session lifetime is host-managed: by default sessions terminate on app exit; persistence across restart is explicitly out of MVP scope (see Pending Decisions).
    - Positive: app quit cleanly tears down all PTY child processes.
    - Negative: after force-kill of the app, no orphan PTY children persist beyond OS process-tree cleanup.

- **AC-3: Notification surface delivers completion and needs-attention alerts without spam.**
  - AC-3.1: A terminal completion event triggers exactly one native notification and one update to the menubar tray window.
    - Positive: a long `sleep 10 && echo done` produces one banner + one tray entry on completion.
    - Negative: a tight loop of fast-completing commands does not produce more than one notification per dedup window per terminal.
  - AC-3.2: Needs-attention events (e.g., prompt-waiting, agent marker, nonzero exit) are differentiated from plain completion in the tray UI.
    - Positive: a non-zero exit shows distinct visual treatment from a zero exit in the tray.
    - Negative: rapid alternating events do not flicker the tray window; throttle is applied.
  - AC-3.3: macOS notification permission denial degrades gracefully.
    - Positive: launching with notifications denied shows an in-app banner explaining the fallback and continues to drive the tray window.
    - Negative: a denied permission does not cause a runtime panic or repeated permission re-prompts.

- **AC-4: Gmail plugin supports multiple independent accounts with persistent incremental sync.**
  - AC-4.1: At least two Gmail accounts can be added via OAuth and their inboxes are independently visible; account switching is explicit and shows the active account in the UI.
    - Positive: adding two accounts shows both as switchable identities; each shows its own inbox.
    - Negative: revoking account A externally produces a typed re-auth prompt for A without affecting B.
  - AC-4.2: Sync state per account is keyed by account ID; incremental sync uses Gmail historyId so restart does not trigger a full re-sync.
    - Positive: app restart resumes sync from the last historyId; only new messages are fetched.
    - Negative: a corrupted/invalid historyId triggers a controlled re-sync, not a crash.
  - AC-4.3: OAuth refresh tokens are stored only via Stronghold (or OS keychain), never in SQLite or frontend storage.
    - Positive: dumping the plugin's SQLite reveals no token strings.
    - Negative: localStorage / IndexedDB / sessionStorage in the WebView contain no token strings.

- **AC-5: Papers plugin reads Zotero, fetches arXiv, and deduplicates merged results.**
  - AC-5.1: Zotero items are read via the Zotero local HTTP API when the Zotero desktop app is running; SQLite read-only is the offline fallback.
    - Positive: with Zotero running, items are visible in the tab; the HTTP API path is taken.
    - Negative: with Zotero closed, the UI shows an "offline — using cached snapshot" indicator and still renders the last-known items.
  - AC-5.2: arXiv metadata is fetched via `arxiv-rs` (or REST equivalent) and merged into the Papers list with deduplication by DOI/arXiv ID against Zotero items.
    - Positive: an arXiv item already in Zotero appears once with a "in-library" badge, not as two separate cards.
    - Negative: deduplication does not collapse legitimately distinct items that share an author/title prefix.
  - AC-5.3: arXiv query/recommendation source is rule-based for MVP (e.g., by category and Zotero tag affinity); personalized ML-style ranking is out of MVP scope.
    - Positive: configuring an arXiv category subscription produces fresh items in the tab.
    - Negative: changing the rule does not require restarting the app.

- **AC-6: Application shell handles cold-start and corruption paths.**
  - AC-6.1: Three MVP tabs render on first launch with empty/placeholder state; no plugin failure prevents the others from rendering.
    - Positive: first launch (no DB, no tokens) shows three tabs, each with its own onboarding/empty state.
    - Negative: Gmail plugin failing to initialize does not prevent Terminal Mesh and Papers from rendering.
  - AC-6.2: SQLite corruption or missing files trigger plugin-scoped recovery UI; the host does not crash.
    - Positive: deleting a single plugin's DB and relaunching shows that plugin's "fresh state" onboarding.
    - Negative: corrupting one plugin's DB does not break other plugins.

## Path Boundaries

### Upper Bound (Maximum Acceptable Scope)
The MVP ships three polished plugin tabs (Terminal Mesh, Gmail multi-account with send/reply, Papers with Zotero HTTP+SQLite dual path and rule-based arXiv recommendations), a fully declarative plugin contract (manifest + typed IPC + per-plugin DB namespaces + permission gating + Stronghold secrets), a dedup-aware menubar tray + native notification surface, plugin-scoped recovery UIs, and a signed/notarized macOS `.app` produced by `tauri build` with a shared design system across plugins.

### Lower Bound (Minimum Acceptable Scope)
All three MVP tabs render and validate the plugin contract end-to-end. Terminal Mesh supports ≥2 concurrent PTYs with streaming/resize/input/exit. Gmail supports ≥1 account read-only with tokens in Stronghold and incremental sync. Papers reads Zotero via one path (HTTP API or SQLite, not both) and lists arXiv items via a single query source. Notification surface fires at least one path (native OR tray) on terminal completion. Plugin contract enforces manifest, typed IPC, and per-plugin DB namespace; permission gating may be deferred to a runtime warning if not full gating.

### Allowed Choices
- Can use: Tauri v2, React 18+ / TypeScript, Rust 2024 edition; `portable-pty`, `xterm.js`, `tauri-plugin-notification`, `tauri-plugin-positioner`, `tauri-plugin-stronghold`, `rusqlite`; for Gmail: `google-gmail1` (Google REST) preferred OR `rust-imap` (IMAP fallback); for arXiv: `arxiv-rs` OR direct REST; for Zotero: local HTTP API primary OR SQLite read-only fallback; for IPC type-codegen: `ts-rs`, `specta`, or equivalent.
- Cannot use: Electron, pure-web SPA without desktop shell, SwiftUI/AppKit-native, tmux-as-system-of-record, notebook-cell UI paradigm, MCP-server-only architecture. (These were evaluated during gen-idea and explicitly rejected.)

## Feasibility Hints and Suggestions

### Conceptual Approach

1. **Host skeleton** (Tauri v2 scaffold + React/TS shell + tab router) is built first as the foundation. The shell exposes a "plugin host" trait that future plugin adapters implement.
2. **Plugin contract** is formalized as Rust traits + a JSON-schema for manifests + a type-generation pipeline that emits TS bindings for IPC commands/events/errors from Rust definitions. The PluginHost loads manifests at compile time for MVP (runtime discovery is Phase 2).
3. **Per-plugin SQLite namespace** is implemented as a single `rusqlite` connection pool that scopes all queries by a `plugin_id` ATTACHed-DB pattern OR by a per-plugin DB file under `${APP_DATA}/plugins/<id>/state.sqlite`. Migrations are owned by the plugin via `refinery` or `sqlx::migrate!`.
4. **Stronghold integration** stores OAuth refresh tokens keyed by `(plugin_id, account_id)`. Access tokens stay in process memory.
5. **Terminal Mesh** wraps `portable-pty` in an actor (one Tokio task per PTY) emitting `TerminalEvent::Output | Resize | Exit | NeedsAttention` events. xterm.js subscribes via `tauri::listen`.
6. **Notification surface** is a host-owned service that subscribes to plugin events with the `notify` capability and applies a host-side dedup window (e.g., 2 s per `(plugin_id, event_id)` tuple) before firing native + tray.
7. **Gmail plugin** implements the OAuth flow with the system browser (Tauri's URL handler) and stores refresh tokens in Stronghold; sync is incremental via `historyId` with a fallback full-sync on `historyNotFound` (HTTP 404).
8. **Papers plugin** queries Zotero HTTP API at `localhost:23119` when Zotero is running (probe on startup); falls back to SQLite read-only on `zotero.sqlite` with `?mode=ro&immutable=1`. arXiv items are fetched via `arxiv-rs` for matching categories; merge by DOI/arXiv ID against the Zotero set.

### Relevant References

- Tauri v2 architecture: https://v2.tauri.app/concept/architecture/
- Tauri plugin development: https://v2.tauri.app/develop/plugins/
- Tauri sidecar: https://v2.tauri.app/develop/sidecar/
- xterm.js: https://github.com/xtermjs/xterm.js
- portable-pty: https://docs.rs/portable-pty
- tauri-plugin-pty: https://lib.rs/crates/tauri-plugin-pty
- google-gmail1 crate: https://docs.rs/google-gmail1
- rust-imap (fallback): https://github.com/jonhoo/rust-imap
- arxiv-rs: https://docs.rs/arxiv-rs
- Zotero direct SQLite: https://www.zotero.org/support/dev/client_coding/direct_sqlite_database_access
- Zotero Web API v3: https://www.zotero.org/support/dev/web_api/v3/basics
- Working precedent — Terax AI (Tauri v2 + xterm.js + portable-pty): https://github.com/crynta/terax-ai
- Plugin contract design references: VS Code Extension API, Obsidian Plugin API (Component lifecycle).

## Dependencies and Sequence

### Milestones

1. **Plugin Contract Foundation** — establishes the host before any plugin is written.
   - Phase A: Tauri v2 scaffold + React/TS shell + tab router with placeholder empty state.
   - Phase B: manifest schema + compile-time plugin registry.
   - Phase C: typed IPC code-gen pipeline (Rust source-of-truth → TS bindings).
   - Phase D: per-plugin SQLite namespace + migration framework.
   - Phase E: tauri-plugin-stronghold integration for OAuth tokens.
   - Phase F: permission gating in the IPC layer.

2. **Terminal Mesh** — first plugin, validates the contract end-to-end and unblocks the notification surface.
   - Phase A: portable-pty Rust actor (stream/resize/input/exit/needs-attention events).
   - Phase B: xterm.js frontend with multi-tab terminal UI + state-store wiring.
   - Phase C: event taxonomy for completion vs needs-attention (consult Codex via `analyze`).
   - Phase D: tray window + native notification with host-side dedup/throttle.
   - Phase E: permission-denial graceful fallback.

3. **Gmail Plugin** — depends on Stronghold and the typed IPC contract.
   - Phase A: OAuth flow per account + Stronghold token storage.
   - Phase B: account-keyed sync DB + historyId-based incremental sync (consult Codex on edge cases).
   - Phase C: inbox UI + account switcher + revoke/re-auth UX.

4. **Papers Plugin (Zotero + arXiv)** — depends on the SQLite framework.
   - Phase A: Zotero local HTTP API probe + client; SQLite read-only fallback.
   - Phase B: arXiv query + rule-based recommendation engine.
   - Phase C: merged list UI + dedup + offline indicator.

Dependencies (relative, not time-based):
- Milestone 1 must be largely complete before Milestone 2 starts (plugin contract is the substrate).
- Milestone 2 (Terminal Mesh) feeds back into Milestone 1 by stress-testing the contract; revisions to Milestone 1 are expected during Milestone 2.
- Milestones 3 and 4 can proceed in parallel once Milestone 1 stabilizes and Milestone 2 has shaken out the IPC/DB/Stronghold patterns.

## Task Breakdown

| Task ID | Description | Target AC | Tag (`coding`/`analyze`) | Depends On |
|---------|-------------|-----------|---------------------------|------------|
| task1 | Scaffold Tauri v2 + React/TS shell with empty 3-tab UI router | AC-6.1 | coding | - |
| task2 | Design typed IPC contract (commands, events, errors) with Rust SoT + TS codegen via ts-rs or specta | AC-1.2 | analyze | task1 |
| task3 | Implement plugin manifest loader and compile-time registry | AC-1.1 | coding | task2 |
| task4 | Implement per-plugin SQLite namespace + migration framework (refinery or sqlx::migrate) | AC-1.3 | coding | task1 |
| task5 | Integrate tauri-plugin-stronghold for OAuth refresh-token storage keyed by (plugin_id, account_id) | AC-4.3 | coding | task1 |
| task6 | Implement permission gating in the IPC layer enforcing manifest-declared permissions | AC-1.4 | coding | task2, task3 |
| task7 | Wrap portable-pty in a Tokio-actor with TerminalEvent stream (output/resize/exit/needs-attention) | AC-2.2 | coding | task2 |
| task8 | Build xterm.js multi-tab frontend with PTY backend wiring; concurrent-PTY stress test | AC-2.1, AC-2.3 | coding | task7 |
| task9 | Define terminal needs-attention taxonomy (prompt-waiting, nonzero exit, stderr burst, agent marker) | AC-3.2 | analyze | task8 |
| task10 | Implement notification surface: native banner + menubar tray window with host-side dedup/throttle | AC-3.1, AC-3.2 | coding | task9 |
| task11 | Handle macOS notification permission denial with in-app fallback banner | AC-3.3 | coding | task10 |
| task12 | Implement Gmail OAuth flow (system browser) with per-account Stronghold token storage | AC-4.1 | coding | task5 |
| task13 | Design Gmail incremental sync strategy (historyId, historyNotFound recovery, rate-limit backoff) | AC-4.2 | analyze | task12 |
| task14 | Implement Gmail sync DB + incremental sync engine per the analyze recommendation | AC-4.2 | coding | task13, task4 |
| task15 | Build Gmail inbox UI + account switcher + re-auth UX | AC-4.1 | coding | task14 |
| task16 | Implement Zotero local HTTP API client (probe localhost:23119) with SQLite read-only fallback | AC-5.1 | coding | task4 |
| task17 | Implement arxiv-rs query + rule-based recommendation engine | AC-5.2, AC-5.3 | coding | task16 |
| task18 | Build Papers tab UI with merge/dedup against Zotero + offline indicator | AC-5.1, AC-5.2, AC-5.3 | coding | task17 |
| task19 | Implement plugin-scoped SQLite corruption recovery + per-plugin reset UX | AC-6.2 | coding | task4 |

## Claude-Codex Deliberation (post round 1)

### Agreements
- Plugin contract must be formalized as manifest + typed IPC + per-plugin DB namespace + permission gating before any plugin is written.
- PTY lifecycle is a major risk area; Tokio-actor-per-PTY pattern is the right shape.
- Gmail multi-account requires consistent `account_id` keying across token storage, sync DB rows, and UI state.
- Zotero strategy: HTTP API primary (when Zotero desktop running), SQLite read-only fallback.
- Notification dedup/throttle is host-owned, not plugin-owned, to prevent multi-source spam.
- OAuth refresh tokens live in Stronghold/keychain only; SQLite holds only non-secret metadata.
- Compile-time plugin registry is realistic for MVP; runtime discovery is Phase 2.

### Resolved Disagreements (Claude positions)
- **IPC schema generation**: Generated TS/Rust types (via ts-rs/specta) over hand-written `invoke`/`listen` strings. Rationale: catches contract drift at compile time and is non-negotiable for the "future plugins without host changes" requirement.
- **Terminal PTY runtime placement**: In-process Rust (Tokio actor) for MVP, not sidecar binary. Rationale: lower packaging surface (no separate signed sidecar), and `portable-pty` is well-isolated. Sidecar pattern is the Phase-2 evolution if isolation becomes necessary.
- **Gmail API choice**: `google-gmail1` (REST) over `rust-imap`. Rationale: native support for `historyId`-based incremental sync, labels, and batch operations; IMAP loses Gmail-specific semantics.
- **Plugin install model for MVP**: Build-time/compile-time only. User-installable plugins are Phase 2 and require sandboxing + signing.

### Convergence Status
- Round: 1 (initial candidate). Pending round-2 review by second Codex pass.

## Pending User Decisions (collected from Codex QUESTIONS_FOR_USER)

- DEC-1: Terminal session persistence across app restart.
  - Claude Position: out of MVP scope (sessions terminate on app exit); persistence is a future enhancement.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: persistence requires either a session daemon (tmux-style) or PTY-state serialization; both add packaging and complexity. Default to no-persistence for MVP.
  - Decision Status: PENDING

- DEC-2: Exact terminal events that trigger 灵动岛-style alert.
  - Claude Position: completion (exit code), needs-attention (prompt waiting / nonzero exit / explicit agent marker via OSC sequence).
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: too few triggers → users miss state; too many → notification fatigue. Default to the four above with host-side dedup.
  - Decision Status: PENDING

- DEC-3: Gmail MVP scope — read-only vs read+modify vs read+send.
  - Claude Position: read + modify (label/archive/mark-read) in MVP; send/reply deferred to Phase 2.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: send/reply adds compose UX and draft management which is non-trivial. Read+modify covers the "triage" use case which is the primary stated need.
  - Decision Status: PENDING

- DEC-4: Gmail OAuth scopes acceptable.
  - Claude Position: `gmail.modify` (read + label + archive + delete; no send).
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: tied to DEC-3.
  - Decision Status: PENDING

- DEC-5: Attachment handling in Gmail.
  - Claude Position: list attachments inline with the message; open-on-demand via the system's default app; no auto-download or indexing in MVP.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: indexing costs DB space and processing; on-demand keeps MVP simple.
  - Decision Status: PENDING

- DEC-6: Does Papers MVP require the Zotero desktop app to be running?
  - Claude Position: no — HTTP API when running, SQLite read-only fallback when closed.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: dual-path adds code; single-path forces user to keep Zotero running.
  - Decision Status: PENDING

- DEC-7: arXiv recommendations — rule-based vs personalized ranking.
  - Claude Position: rule-based for MVP (categories + Zotero tag affinity); personalized ranking deferred.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: personalized ranking needs an embedding pipeline; out of scope.
  - Decision Status: PENDING

- DEC-8: Cross-plugin global search (e.g., search across Gmail + Zotero + terminal scrollback).
  - Claude Position: out of MVP scope; deferred.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: cross-plugin index is a non-trivial schema decision; defer until plugin contract has matured.
  - Decision Status: PENDING

- DEC-9: Plugin install model — build-time vs user-installable.
  - Claude Position: build-time only for MVP; user-installable plugins are Phase 2 with sandboxing.
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: user-installable plugins require capability sandboxing and signing — too much surface for MVP.
  - Decision Status: PENDING

- DEC-10: Local-DB encryption posture — encrypted DBs everywhere, or only secrets in Stronghold.
  - Claude Position: only secrets in Stronghold; plain SQLite for non-secret state (subject to FileVault on macOS).
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: full DB encryption adds key-management complexity; FileVault covers the at-rest threat model for a personal desktop tool.
  - Decision Status: PENDING

- DEC-11: I18n posture — English-only first, or CN/EN mixed from day one.
  - Claude Position: CN/EN bilingual UI strings from day one (user's communication is in Chinese; mixed content is the actual workflow).
  - Codex Position: N/A — open question raised by Codex.
  - Tradeoff Summary: bilingual at MVP is cheap if planned now; retrofitting is more expensive.
  - Decision Status: PENDING

## Implementation Notes

### Code Style Requirements
- Implementation code and comments must NOT contain plan-specific terminology such as "AC-", "Milestone", "Step", "Phase", or similar workflow markers.
- These terms are for plan documentation only, not for the resulting codebase.
- Use descriptive, domain-appropriate naming in code instead (e.g., the host trait is `PluginHost`, not `MilestoneOnePluginHost`).
```

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_00-07-14
- Tool: codex
