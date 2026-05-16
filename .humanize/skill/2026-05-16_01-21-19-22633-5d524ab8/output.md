## AGREE
- AC-1.1/AC-1.2 and task3 are reasonable: manifest-driven plugin discovery plus generated Rust/TS registries is necessary to avoid hand-maintained host routing.
- AC-2.1 and task10 are correct: MCP sidecar stdout must be protocol-only, with logs restricted to stderr/files.
- AC-2.2/AC-2.3 and task13 are reasonable: per-tab MCP config plus GUI-safe `claude` PATH discovery are required for Finder-launched macOS apps.
- AC-4.1/AC-4.4 and tasks15-17 are reasonable: ≥4 PTYs, 1 MB rings, and canonical workspace uniqueness are concrete and testable.
- AC-5.2/AC-5.4 and tasks29/6 correctly treat Stronghold token storage and interrupted setup recovery as MVP requirements.
- AC-7.1-AC-7.5 and tasks24-26 are reasonable: global confirm-on-write queue, idempotency, and scope honesty are well-scoped.
- AC-9.4/AC-9.5 and tasks37-38 are reasonable: quit sequencing and structured redacted logs are necessary for this architecture.

## DISAGREE
- AC-1.5 / Feasibility Hint 4: the stated `PluginCapability` is not implementable as written. A React closure cannot hold an `Arc<PluginCapability>` from Rust, and any capability passed through Tauri IPC must be represented by serialized data. “Opaque, not JSON-serializable” conflicts with frontend-to-Rust IPC mechanics.
- AC-1.2: “Generated outputs land at known checked-in paths (gitignored)” is internally contradictory. A file cannot be both checked in and gitignored as the normal source of truth.
- task6: target AC includes AC-4.3, but Stronghold setup has no direct relationship to workspace directory binding. This is a traceability error.
- task7: target AC includes AC-6.1, but frontend plugin lifecycle hooks and error boundaries do not implement Zotero three-mode access. This is a traceability error.
- AC-1.3 positive test: “plugin A and plugin B can run migrations concurrently” does not prove the risky case. The real contention is multiple sidecar copies for the same plugin/database under N×M lifecycle.
- AC-3.4 positive test: “scanning host process memory” is not a reliable acceptance test. It is brittle and may produce false confidence or false failures.
- AC-9.4 negative test: “force-quitting via OS still triggers SIGTERM cleanup hooks where possible” is not generally guaranteed for force-quit/kill paths. The criterion should not require behavior the process may not receive a chance to perform.

## REQUIRED_CHANGES
- Edit AC-1.5 and task4/task7 to define a feasible capability transport: frontend wrappers may carry an opaque branded capability handle/nonce, but the authoritative capability state must live in Rust keyed by `(plugin_id, mount_id)`. Runtime validation must reject forged, expired, wrong-mount, or missing handles.
- Edit AC-1.2 wording to replace “checked-in paths (gitignored)” with “known repo-local generated paths that are gitignored and regenerated during build,” or explicitly choose checked-in generated files.
- Edit AC-1.3 positive/negative tests to include same-plugin concurrent sidecar access: multiple Gmail sidecars opening `${APP_DATA}/plugins/gmail/state.sqlite` must not race migrations or hit avoidable `database is locked` failures.
- Fix task6 target AC from `AC-4.3, AC-5.4` to `AC-5.2, AC-5.4`.
- Fix task7 target AC from `AC-1.5, AC-6.1` to `AC-1.5` plus whichever shell/error-boundary AC it actually supports, likely `AC-1.7` or `AC-9.3`.
- Add an explicit task for Gmail modify operations required by AC-7.1 and the locked `gmail.modify` scope. Current tasks cover send and inbox UI, but not label/archive/mark-read/mark-unread/trash behavior despite AC-7.1 using `gmail.label`.
- Edit AC-3.4 token-leak testing to use deterministic checks: no tokens in orchestrator env/config/stdin/stdout/logs/WebView storage, and code path asserts orchestrator lacks Stronghold access. Keep memory scanning only as optional diagnostic, not required AC.
- Edit AC-9.4 negative to distinguish graceful quit from OS force-quit. Required AC should be: normal quit leaves no orphans; startup cleanup detects/reaps stale app-owned process records where possible. Do not promise cleanup hooks on uncatchable termination.
- Add explicit acceptance coverage for no Gmail send/reply UI button in MVP, because it is locked but the Gmail UI task32 could accidentally include compose/send affordances.

## OPTIONAL_IMPROVEMENTS
- Add a manifest uniqueness check to AC-1.1: duplicate `plugin_id`, duplicate `db_namespace`, or duplicate `command_bin` should fail build with a typed error.
- Add per-command permission metadata generation to AC-1.2/AC-1.4 so TS wrappers and Rust dispatcher share the same command permission source of truth.
- In AC-4.1, replace `/var/log/system.log` with a portable synthetic fast-output command for tests, since macOS permissions and file presence vary.
- In AC-5.3, specify the 429 backoff ceiling and reset behavior in the AC, not only in the spec task.
- In AC-5.6, define the attachment temp cleanup window numerically.
- In AC-6.1, specify Zotero SQLite path discovery behavior and user override handling.
- In AC-8.1, clarify whether native notifications require user permission request during first run or lazily on first event.
- In task sequencing, move task37 logging earlier or split it: correlation IDs are referenced by task4/task11 tests before task37 currently runs.

## UNRESOLVED
- None.
