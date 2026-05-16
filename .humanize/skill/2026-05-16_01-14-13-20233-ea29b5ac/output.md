## CORE_RISKS
- `build.rs` codegen can become brittle if it writes frontend files outside Cargo’s normal output model; stale generated TS/Rust registries are a high-risk source of confusing compile/runtime mismatch.
- Tauri sidecar packaging is easy to under-specify: dev mode, release bundles, notarization, binary naming, target triples, and per-plugin sidecar discovery must all work consistently.
- MCP stdio lifecycle needs strict framing and process supervision; a sidecar that writes logs to stdout can corrupt protocol traffic unless logs are forced to stderr or structured files.
- Confirm-on-write can deadlock if plugin sidecars synchronously wait for host approval while the host UI is blocked, unmounted, or already showing another write modal.
- Capability rotation has race risk during React strict-mode double mounts, hot reload, tab close/reopen, and in-flight IPC calls using a soon-to-be-invalid token.
- Multi-process SQLite WAL access needs explicit `busy_timeout`, retry policy, schema migration locking, and read/write ownership per plugin to avoid intermittent `database is locked`.
- Stronghold setup/interruption is risky: first-run password creation, unlock failure, corrupted vault, forgotten password, and partially stored OAuth account state need deterministic recovery flows.
- Orchestrator privilege boundaries depend on minting capabilities only in Rust dispatcher state; any frontend-visible representation that can be replayed across mount IDs weakens the model.
- `claude` launched from a macOS GUI app may not inherit the user’s shell `PATH`, so CLI discovery and environment construction must be handled explicitly.
- Workspace uniqueness and tab restore can race across app restart, stale metadata, symlinks, case-insensitive paths, and user-picked directories that resolve to the same canonical path.

## MISSING_REQUIREMENTS
- First-run filesystem bootstrap: create `~/AgentPlatform/`, `~/AgentPlatform/workspaces/`, app data dirs, plugin DB dirs, log dirs, and handle permission failures.
- Plugin manifest schema: required fields, versioning, declared commands/tools/resources, frontend entry path, sidecar binary name, permissions, migrations path, and validation errors.
- Behavior when no plugin sidecars are built or bundled: app should show a developer-facing diagnostics page instead of failing silently.
- Stronghold lifecycle: initial password setup, unlock prompt timing, retry limits, vault reset UX, interrupted setup recovery, and account-token cleanup when OAuth fails halfway.
- OAuth redirect mechanics for Gmail: loopback port selection, browser launch failure, user cancels consent, duplicate account detection, token refresh failure, and account removal.
- `claude` environment generation: where temporary MCP config files live, how per-tab MCP server commands are injected, cleanup on tab close, and minimum supported CLI version detection.
- Sidecar graceful shutdown contract: host-to-sidecar shutdown signal, timeout before kill, cleanup of child processes, and behavior during app quit.
- Logging/diagnostics: per-process log locations, correlation IDs for tab/plugin/write request, redaction rules for OAuth payloads and email bodies, and user-visible diagnostics export.
- Migration failure behavior: block plugin mount, rollback expectations, backup policy for SQLite before destructive migrations, and how plugin reset works.
- Workspace metadata store: canonical path, display name, created/last-used timestamps, open-tab lock state, transcript count cache, and stale/missing directory handling.
- File-write confirmation boundary: draft says file writes outside `.claude/`; implementation must define how the platform observes/intercepts those writes from `claude`, or document that only plugin-mediated writes are enforceable in MVP.
- Notification permission bootstrap: when to request permission, what happens before permission is granted, and how tray/in-app fallback dedup interacts with native notification dedup.

## TECHNICAL_GAPS
- Codegen ownership is underspecified: Cargo `OUT_DIR` output is not directly importable by Vite; generated TS probably needs a checked-in/generated source dir with clean/regenerate rules.
- Need a concrete dispatcher contract: typed command naming, request envelope shape, plugin ID/mount ID extraction, permission lookup, error normalization, and frontend wrapper generation.
- MCP Rust SDK choice affects tool schema generation, stdio transport maturity, cancellation support, request IDs, and compatibility with Claude Code’s MCP implementation.
- Need an MCP config generator per `claude` tab that starts exactly one sidecar copy per plugin for that tab and passes tab/workspace identity to each sidecar.
- Sidecar identity must be explicit: host-owned UI sidecars and per-claude sidecars need distinct client IDs, log prefixes, DB connection modes, and capability scopes.
- Confirm-on-write needs a queue/arbiter: concurrent write requests from multiple tabs should serialize or show a managed queue with cancel/timeouts, not stack modals.
- Approval results need idempotency: if the sidecar retries after host approval, the write request must carry an operation ID to prevent duplicate sends/label changes/stdin writes.
- Terminal stdin confirmation is especially sensitive: the PTY actor must pause the write until approval without blocking output handling, resize events, or process exit observation.
- Ring buffer implementation needs byte/UTF-8 boundary policy, ANSI escape handling, snapshot read semantics, and overflow coalescing that preserves terminal render correctness.
- Cross-tab scrollback read needs bounded APIs: max bytes, redaction policy if any, stale tab behavior, and read consistency while output is actively streaming.
- Papers Zotero SQLite fallback needs file discovery, immutable connection behavior on macOS, detection of locked/corrupt DB, and cache invalidation when live Zotero becomes available.
- Gmail incremental sync needs initial full-sync boundaries, page token persistence, historyId advancement only after successful processing, and controlled resync progress UX.
- App quit sequencing is missing: stop accepting new writes, resolve/cancel pending confirmation modals, terminate PTYs, gracefully stop sidecars, then force-kill after timeout.

## ALTERNATIVE_DIRECTIONS
- `rmcp` vs hand-written MCP stdio server: `rmcp` reduces protocol boilerplate; hand-written gives tighter control if SDK maturity or Claude compatibility becomes an issue.
- `specta` vs `ts-rs`: `specta` is stronger for command/event schemas across Tauri-style APIs; `ts-rs` is simpler for Rust type-to-TypeScript generation.
- `refinery` vs `sqlx::migrate!`: `refinery` fits plugin-owned migration folders; `sqlx::migrate!` gives compile-time embedding but can be awkward with dynamically discovered plugins.
- Zustand vs React Context + reducer: Zustand is pragmatic for multi-tab app state and event streams; Context-only is lighter but may become noisy under terminal/Gmail/Papers updates.
- Generated source committed vs generated only at build time: committed generated files improve frontend tooling and reviewability; build-only avoids churn but complicates Vite/editor integration.
- JSON structured logs vs human-readable rolling logs: JSON supports correlation and diagnostics export; human logs are easier during early local debugging.
- One write-confirmation global queue vs per-plugin queues: global queue is simpler and safer; per-plugin queues preserve throughput but require stronger conflict handling.
- SQLite connection pool per sidecar vs single actor-owned connection: pools improve concurrent reads; actor-owned writes simplify WAL locking and migration safety.

## QUESTIONS_FOR_USER
- None — all user-facing decisions are locked in the draft.

## CANDIDATE_CRITERIA
- AC: A fresh checkout with only `.git/` and `.humanize/` can run the bootstrap/dev command and produce a Tauri app with generated Rust registry, TS wrappers, and frontend tab registry from `plugins/*/plugin.toml`.
- AC: Adding a minimal sample plugin directory and rebuilding makes its tab/tool registration appear without editing host router, tab enum, or dispatcher source.
- AC: In release bundle validation, every configured plugin sidecar binary is present, executable, version-reportable, and addressable by Tauri sidecar APIs.
- AC: Killing one plugin sidecar triggers restart attempts at 1s, 2s, 4s up to 60s cap, logs each attempt with correlation IDs, shows plugin-local error UI, and does not interrupt other plugins.
- AC: Two concurrent write requests from different `claude` tabs produce deterministic confirm-on-write handling; approving one and canceling the other performs exactly one external write.
- AC: Capability tokens rotate on plugin unmount/remount; stale tokens fail at the Rust dispatcher, including during React strict-mode double mount and in-flight IPC race tests.
- AC: If `claude` is absent from GUI app PATH, orchestrator tab shows Chinese onboarding with detected search paths and does not create sidecar processes.
- AC: Four simultaneous PTY sessions run under distinct workspace directories, each maintains a 1 MB bounded ring buffer, and overflow coalesces without crashing or unbounded memory growth.
- AC: Workspace creation on first launch creates `~/AgentPlatform/workspaces/<name>/`; picking an already-open canonical path focuses the existing tab instead of opening a duplicate.
- AC: Gmail plugin can authorize and sync at least two accounts, stores refresh tokens only in Stronghold, keeps account state keyed by `account_id`, and recovers from `historyNotFound` via controlled resync.
- AC: Papers plugin resolves Zotero in priority order: live HTTP, read-only immutable SQLite, then cached snapshot; each fallback path has visible status and test coverage.
- AC: Notification denial produces an in-app banner fallback while tray notification UI still works; dedup suppresses repeated `(plugin_id, terminal_id, event_kind)` events within roughly 2 seconds.
- AC: App quit cancels pending confirmation modals, terminates PTYs, sends graceful shutdown to all sidecars, force-kills after timeout, and leaves no orphan sidecar processes.
- AC: Stronghold setup interrupted mid-flow can be resumed or reset without leaving partially authorized Gmail accounts or plaintext tokens in SQLite/config.
