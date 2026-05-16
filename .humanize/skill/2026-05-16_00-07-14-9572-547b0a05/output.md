## AGREE
- AC-1 is directionally right: manifest + Rust adapter + frontend component + typed IPC is the right host/plugin boundary for later WeChat and GitHub PR review.
- AC-2.1/AC-2.2 are well-grounded: `portable-pty` + xterm.js + one actor/task per PTY is a reasonable MVP architecture.
- AC-2.3 is reasonable: terminal persistence across app restart should stay out of MVP unless the user explicitly chooses otherwise.
- AC-3 is reasonable: notification/tray dedup belongs in the host, not individual plugins.
- AC-4.1/AC-4.2 are correct: Gmail multi-account must key OAuth, sync state, and UI identity by stable account ID; Gmail REST/historyId is the right default.
- AC-4.3 is required and well-scoped: refresh tokens must not touch SQLite or WebView storage.
- AC-5.2/AC-5.3 are reasonable: DOI/arXiv-ID dedup plus rule-based arXiv recommendations is MVP-appropriate.
- AC-6 is important and correctly included: plugin failures and DB corruption must be plugin-scoped, not app-fatal.
- The milestone ordering is mostly sound: contract foundation first, Terminal Mesh as the first stress test, Gmail/Papers afterward.

## DISAGREE
- AC-1 says plugins can be added “without modifying host code,” but Feasibility Hint 2 and task3 say “compile-time plugin registry.” A compile-time registry is fine only if adding a plugin means adding a manifest/plugin crate entry outside host source. If it requires editing a Rust match statement, command registration list, tab enum, or router, it violates the locked plugin-host goal.
- AC-1.2 overclaims that raw `invoke('plugin.<name>.<command>', args)` is compile-time type-checked. Raw Tauri `invoke` strings are not type-safe by themselves. The plan needs generated TS client functions or a generated command map; otherwise this acceptance criterion is not actually testable.
- AC-1.3 overclaims SQLite isolation. A shared `rusqlite` connection with namespaced tables does not prevent migrations or plugin SQL from querying another plugin’s tables. SQLite isolation needs per-plugin DB files, separate connections, an authorizer, or no raw SQL access from plugins.
- AC-1.4 conflicts with the Lower Bound, which says permission gating may be deferred to a runtime warning. Permission gating is core to the future WeChat/GitHub plugin contract; a warning-only system would bake in the wrong security model.
- AC-5.1 has a mismatch: it says SQLite read-only is the offline fallback, but the negative case expects “cached snapshot” when Zotero is closed. Those are different behaviors. Reading Zotero’s SQLite directly is not the same as using the app’s own cached snapshot.
- Upper Bound includes Gmail “send/reply,” but DEC-3 says Claude’s position is read + modify with send/reply deferred. That is an internal contradiction.
- task9 proposes “stderr burst” as a needs-attention trigger, but AC-3.2 examples do not include it. That trigger is high-risk for notification spam and false positives unless the user explicitly wants it.
- The plan says “consult Codex via `analyze`” in task9/task13. That is process-language, not an implementation task. The final plan should name the concrete analysis deliverable instead.

## REQUIRED_CHANGES
- Change AC-1 and task3 to define the exact build-time plugin registration mechanism: adding a plugin must require only a manifest/plugin crate/frontend entry in a plugin directory or registry data file, not edits to host source code, host router enums, command match arms, or tab switch statements.
- Change AC-1.2 to require generated typed TS wrappers, e.g. `plugins.gmail.commands.listMessages(args)`, rather than claiming raw `invoke(...)` calls are type-checked.
- Change AC-1.3 and Feasibility Hint 3 to choose enforceable DB isolation. Prefer per-plugin DB files under `${APP_DATA}/plugins/<id>/state.sqlite` with separate connections and migration runners; do not present table-prefixing/shared connection as equivalent isolation.
- Change the Lower Bound to keep permission gating mandatory. It can be minimal, but it cannot be warning-only if AC-1.4 is an acceptance criterion.
- Change AC-5.1 to distinguish the two fallback modes: “Zotero closed but SQLite readable” versus “Zotero unavailable/corrupt, use app cached snapshot.” Add acceptance tests for both or remove the cached-snapshot wording.
- Fix the Upper Bound / DEC-3 conflict: either Upper Bound says Gmail read + modify only, or DEC-3 says send/reply is in MVP. Pick one before convergence.
- Change task9 to remove `stderr burst` from the default needs-attention taxonomy unless explicitly approved. Keep default triggers to exit completion, nonzero exit, prompt-waiting if reliably detected, and explicit agent marker.
- Replace “consult Codex via `analyze`” in task9/task13 with concrete outputs, such as “write terminal event taxonomy spec” and “write Gmail sync edge-case spec.”

## OPTIONAL_IMPROVEMENTS
- Add an explicit frontend plugin interface AC under AC-1: lifecycle hooks, tab metadata, empty/error states, event subscriptions, and capability declarations.
- Add an AC for cancellation/backpressure in event streams, especially PTY output and Gmail sync progress.
- Add an AC for Gmail rate limiting and backoff, currently only mentioned in task13.
- Add a packaging note that signing/notarization is stretch/upper-bound only, not required for local MVP validation.
- Add a small “test strategy” section mapping ACs to unit/integration/manual tests, especially PTY orphan checks, token leakage checks, and DB corruption tests.
- Add a short data-retention policy for terminal scrollback, Gmail cached metadata, and downloaded/opened attachments.
- Add explicit arXiv API politeness constraints: cache results, avoid aggressive polling, and expose refresh cadence.
- Clarify whether Stronghold or OS keychain is the actual MVP target. “Stronghold or OS keychain” is acceptable for planning, but implementation should choose one primary path.

## UNRESOLVED
- Terminal persistence: Claude=out of MVP, Codex=agree default out of MVP, user must choose only if restart persistence is a hard requirement.
- Gmail send/reply: Claude=contradictory, Upper Bound includes send/reply while DEC-3 defers it; Codex=defer send/reply and keep read+modify, user must choose.
- Gmail OAuth scope: Claude=`gmail.modify`, Codex=`gmail.modify` only if modify actions are MVP; otherwise narrower read-only scope, user must choose based on DEC-3.
- Zotero fallback: Claude=HTTP primary plus SQLite read-only fallback plus cached snapshot wording, Codex=separate SQLite fallback from app-owned cache, user must choose required offline behavior.
- I18n posture: Claude=CN/EN bilingual from day one, Codex=acceptable but not required for MVP unless user-facing Chinese is a product requirement, user must choose.
- Local DB encryption: Claude=secrets only in Stronghold, plain SQLite for non-secret state, Codex=same default is reasonable, user must choose if Gmail/Papers cached content should be encrypted beyond FileVault.
