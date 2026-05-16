## CORE_RISKS
- Tauri “plugin” model may be overspecified: dynamic third-party plugin loading is not equivalent to a sandboxed extension runtime.
- PTY sidecar lifecycle is high risk: orphaned shells, reconnect behavior, backpressure, and process cleanup must be designed early.
- Gmail multi-account OAuth isolation can fail silently if token storage, account IDs, and sync DB rows are not keyed consistently.
- Zotero local SQLite access risks corruption or incompatibility if the app reads while Zotero is writing or if schema assumptions drift.
- macOS tray/Dynamic-Island-style notification may not be reliable for urgent attention unless native notifications and tray window state are coordinated.
- Event-heavy IPC can become fragile if terminal streams, Gmail sync events, and paper indexing updates share an untyped ad hoc channel model.
- Plugin extensibility could collapse into tight coupling unless frontend routes, Rust commands, permissions, DB migrations, and events are formalized now.

## MISSING_REQUIREMENTS
- Account model: add/remove Gmail accounts, display names, sync scopes, re-auth, token revocation, and failed-auth UX.
- Offline behavior: what remains usable without network, and how sync resumes after sleep/wake.
- Terminal persistence: whether sessions survive app restart, whether scrollback is persisted, and how completed terminals are retained or archived.
- Attention semantics: exact states that trigger alerts, such as command exit, nonzero exit, prompt waiting, stderr burst, or explicit agent marker.
- Notification throttling: prevent repeated alerts from many terminals or email sync loops.
- Search requirements across Gmail, Zotero, arXiv, and terminal history.
- Data retention and deletion rules per plugin.
- macOS permissions flow for notifications, keychain, local Zotero access, and sidecar execution.
- Import/export/backup expectations for SQLite plugin state.
- UI density and navigation expectations for future tabs, especially plugin ordering, pinning, and unread/attention badges.

## TECHNICAL_GAPS
- Manifest schema is illustrative but not executable: loading model, versioning, permission enforcement, migrations, and frontend registration are undefined.
- IPC contract lacks typed schemas for commands, events, errors, cancellation, retries, and streaming.
- Rust adapter boundary is fuzzy: unclear whether adapters are compiled into the app, sidecars, crates, or separately installed bundles.
- Sidecar packaging/signing/notarization path for portable-pty and future services is not defined.
- Terminal backend needs a protocol for resize, stdin/stdout/stderr, exit status, cwd/env, shell profiles, and reconnect.
- xterm.js integration needs performance limits for scrollback, binary output, large bursts, and Unicode/input method handling.
- Gmail sync strategy is undefined: polling vs push, incremental history IDs, attachment handling, rate limits, and conflict behavior for send/archive/labels.
- Zotero strategy needs a clear priority between local SQLite, local HTTP API, and remote Web API.
- arXiv recommendation/indexing logic is missing: query sources, ranking inputs, deduplication against Zotero, and refresh cadence.
- SQLite “per-plugin keyed state” needs migration ownership, encryption boundaries, connection pooling, and backup/restore behavior.
- Stronghold usage needs explicit token shape, key derivation/unlock behavior, migration, and account deletion semantics.
- Frontend plugin API needs a design system contract, route/tab registration, shared state access, error surfaces, and feature flags.

## ALTERNATIVE_DIRECTIONS
- Gmail via Google REST API crate + OAuth2: strongest Gmail feature coverage, more API complexity.
- Gmail via IMAP fallback: simpler mail read path, weaker Gmail labels/history/send semantics.
- Zotero local HTTP API first: safer than SQLite reads, but depends on Zotero running.
- Zotero SQLite read-only fallback: works offline, but needs schema/version guards and file-lock caution.
- Terminal PTY in-process Rust manager: simpler packaging, higher risk of blocking/crash coupling.
- Terminal PTY sidecar manager: cleaner lifecycle isolation, more packaging/signing work.
- IPC with generated TypeScript/Rust schemas: more upfront setup, fewer runtime contract bugs.
- IPC with hand-written invoke/listen names: faster prototype, easier to drift.
- Plugin registry as compile-time registry: realistic for MVP, less dynamic.
- Plugin registry as runtime manifest discovery: more extensible, harder to secure and package.

## QUESTIONS_FOR_USER
- Should terminal sessions survive app restart, or is live-only state acceptable for MVP?
- What exact terminal events should trigger the 灵动岛-style alert?
- Should Gmail MVP support sending/replying, or only inbox/read/archive/search?
- Which Gmail scopes are acceptable for MVP: read-only, modify, send, or full mail?
- Should attachments be downloaded, indexed, ignored, or opened on demand?
- Is Zotero expected to work when the Zotero desktop app is closed?
- Should arXiv recommendations be rule/query based first, or should the MVP include personalized ranking from Zotero history?
- Should cross-plugin global search be part of MVP or deferred?
- Should future plugins be developer-installed at build time, or user-installable after app release?
- What is the privacy expectation for local databases: plain SQLite metadata, encrypted DBs, or only secrets in Stronghold?
- Should the app optimize for English-only content first, or Chinese/English mixed workflows from day one?

## CANDIDATE_CRITERIA
- A plugin can register one tab, typed commands, typed events, permissions, and a SQLite migration without editing unrelated plugin code.
- The app starts with exactly three MVP tabs: Terminal Mesh, Gmail, and Papers.
- Terminal Mesh can open at least 4 concurrent PTY sessions, stream output to xterm.js, resize correctly, send input, and report exit status.
- A terminal completion or needs-attention condition triggers one native notification and one tray-positioned alert without duplicate spam.
- Gmail supports at least two independently authenticated accounts with separate token storage, separate sync state, and visible account switching.
- Revoking or failing one Gmail account does not break other accounts.
- Gmail sync persists message metadata in SQLite and resumes incrementally after app restart.
- Papers tab can read/import Zotero items, fetch arXiv metadata, deduplicate obvious matches, and persist recommendation/index state.
- OAuth secrets are stored only through Stronghold/keychain-backed storage, not plain SQLite or frontend local storage.
- Each plugin owns its DB namespace/migrations and can be reset without deleting other plugin state.
- The frontend exposes stable tab badges for unread, syncing, error, and needs-attention states.
- macOS notification permission denial is handled with visible in-app status and no crash.
