# Goal Tracker

<!--
This file tracks the ultimate goal, acceptance criteria, and plan evolution.
It prevents goal drift by maintaining a persistent anchor across all rounds.

RULES:
- IMMUTABLE SECTION: Do not modify after initialization
- MUTABLE SECTION: Update each round, but document all changes
- Every task must be in one of: Active, Completed, or Deferred
- Deferred items require explicit justification
-->

## IMMUTABLE SECTION
<!-- Do not modify after initialization -->

### Ultimate Goal
Implement the MVP of a Tauri v2 desktop application whose three plugin tabs (Terminal Mesh, Gmail multi-account, Papers via Zotero+arXiv) run as OS-process-isolated MCP-server sidecars, plus a privileged single-instance orchestrator tab that auto-launches `claude` with full platform context. Each Terminal Mesh tab is bound to one workspace (real directory under `~/AgentPlatform/workspaces/<name>/`, user-visible and IDE-accessible). Long-running terminal/agent state surfaces through a host-owned dedup-aware notification surface combining native macOS notifications and a menubar tray window. All locked decisions in the v2 draft are constraints, not options.

### Acceptance Criteria
<!-- Extracted from .humanize/plans/plan.md; sub-ACs (AC-X.Y) are defined in plan.md with TDD positive/negative tests. -->

- **AC-1**: Plugin Contract Pipeline is declarative and host-agnostic. Adding a plugin requires only `plugins/<id>/` (manifest + Rust adapter + frontend); `build.rs` codegen emits Rust registry + frontend tab registry + per-plugin TS wrappers + per-command permission metadata into gitignored generated dirs without host source edits. (Sub-criteria AC-1.1..AC-1.7 cover manifest schema, typed IPC, per-plugin SQLite isolation, mandatory permission gating, branded-handle `PluginCapability` lifecycle, sidecar identity, exponential-backoff crash recovery.)
- **AC-2**: MCP Stdio Server Discipline + Claude Wiring. (AC-2.1..AC-2.3: stdio framing with logs→stderr/files only; per-tab MCP config generator with cleanup; `claude` PATH discovery without GUI shell inheritance.)
- **AC-3**: Orchestrator is a privileged single-instance host tab. (AC-3.1..AC-3.5: top-left fixed single-instance; auto-launch claude with preconfigured MCP; cross-tab read via Rust-only `cross_tab_read` flag; Gmail send via OAuth delegation never leaking token; semantic-summary notification for claude task complete.)
- **AC-4**: Terminal Mesh + Workspaces. (AC-4.1..AC-4.6: ≥4 concurrent PTYs HARD; 1MB ring buffer per PTY HARD; stdin/resize/exit/cwd/env; workspace dir at `~/AgentPlatform/workspaces/<name>/` or user-pointed existing dir; persistent + canonical-path uniqueness; switcher modal with metadata; Open in Cursor + Reveal in Finder.)
- **AC-5**: Gmail Plugin (multi-account, OAuth, incremental sync, modify operations). (AC-5.1..AC-5.7: ≥2 accounts HARD; OAuth via system browser loopback; Stronghold-only refresh tokens; historyId sync with 5-min 429 ceiling; Stronghold interrupted-setup recovery; send + modify via confirm-on-write with operation IDs; attachments open-on-demand with 60s cleanup; NO compose/send UI buttons.)
- **AC-6**: Papers Plugin (Zotero three-mode + arXiv). (AC-6.1..AC-6.4: Live HTTP → SQLite read-only → cached snapshot fallback with default + user-overridable path; arxiv-rs dedup by DOI/arXiv-ID; rule-based recommendations; ~30 min polite caching DIRECTIONAL.)
- **AC-7**: Confirm-on-Write Surface. (AC-7.1..AC-7.5: every plugin-mediated write gated; host-owned global queue, no stacked modals; idempotent operation IDs; terminal stdin pause without blocking output/resize/exit; scope honesty — raw FS writes inside workspace NOT intercepted in MVP.)
- **AC-8**: Notification Surface ("灵动岛" analog). (AC-8.1..AC-8.4: native macOS notification + menubar tray on completion with lazy permission request; default triggers: exit 0, nonzero exit, OSC agent marker, prompt-waiting (NOT stderr-burst); semantic summary from claude; graceful permission-denial fallback.)
- **AC-9**: Application Shell Robustness + First-Run Bootstrap + Quit Sequencing. (AC-9.1..AC-9.5: first-run dir bootstrap with permission-error UX; dev-diagnostics page when no plugins built; plugin-scoped migration-failure recovery; normal-quit sequenced cleanup + startup stale-process reaping for force-quit recovery; structured JSON logs with correlation IDs and token/email redaction.)

---

## MUTABLE SECTION
<!-- Update each round with justification for changes -->

### Plan Version: 2 (Updated: Round 0 review)

#### Plan Evolution Log
<!-- Document any changes to the plan with justification -->
| Round | Change | Reason | Impact on AC |
|-------|--------|--------|--------------|
| 0 | Initial plan | - | - |
| 0 review | Added explicit active tasks for orchestrator Gmail OAuth delegation and scope-honesty runtime documentation | Review found AC-3.4 and part of AC-7.5 were present in the acceptance criteria and milestone narrative but not directly tracked by any active implementation task | Restores direct task coverage for AC-3.4 and AC-7.5 |

#### Active Tasks
<!-- Mainline tasks only: each task must directly advance the current round objective and carry routing metadata -->
<!-- 38 tasks extracted from .humanize/plans/plan.md ; full descriptions/dependencies in plan.md task table. All start as pending in Round 0. -->

| Task | Target AC | Status | Tag | Owner | Notes |
|------|-----------|--------|-----|-------|-------|
| task1: Scaffold Tauri v2 + React/TS shell with fixed-orchestrator-slot tab strip + first-run bootstrap | AC-9.1 | pending | coding | claude | M1 Phase A |
| task2: Write `docs/specs/plugin-contract.md` | AC-1.1, AC-1.4, AC-1.5 | pending | analyze | codex | M1 Phase B; deps task1 |
| task3: `build.rs` plugin scanner emitting Rust registry + frontend tab registry + per-plugin TS wrappers + per-command permission metadata | AC-1.1, AC-1.2 | pending | coding | claude | M1 Phase C; deps task2 |
| task4: Generated Rust IPC dispatcher with permission gating + PluginCapability validation + structured JSON logs (correlation IDs + OAuth/email redaction) | AC-1.4, AC-1.5, AC-9.5 | pending | coding | claude | M1 Phase D; deps task3 |
| task5: Per-plugin SQLite framework (WAL, busy_timeout=5000, advisory-lock migrations for sibling sidecar copies, plugin-scoped migration recovery UI) | AC-1.3, AC-9.3 | pending | coding | claude | M1 Phase E; deps task1 |
| task6: tauri-plugin-stronghold integration (master-password setup/resume/reset + interrupted-state detection) | AC-5.2, AC-5.4 | pending | coding | claude | M1 Phase F; deps task1 |
| task7: Frontend lifecycle hooks + branded-handle PluginCapability issuance + host error boundary + capability-rotation race safety | AC-1.5 | pending | coding | claude | M1 Phase G; deps task4 |
| task8: Dev diagnostics page (no plugins built) | AC-9.2 | pending | coding | claude | M1 Phase H; deps task1 |
| task9: Write `docs/specs/mcp-sidecar.md` | AC-2.1, AC-1.6 | pending | analyze | codex | M2 Phase A; deps task1 |
| task10: MCP stdio framing helper crate (logs→stderr/files; lint banning println/console.log in sidecar code) | AC-2.1, AC-1.6 | pending | coding | claude | M2 Phase B; deps task9 |
| task11: Sidecar lifecycle manager (exp-backoff auto-restart, graceful Shutdown 5s timeout, distinct host-UI vs per-claude client identities) | AC-1.7, AC-1.6 | pending | coding | claude | M2 Phase C; deps task10 |
| task12: Write `docs/specs/claude-launch.md` | AC-2.3, AC-3.2 | pending | analyze | codex | M2 Phase D; deps task1 |
| task13: `claude` PATH discovery + per-tab MCP config generator + cleanup + onboarding card | AC-2.2, AC-2.3, AC-3.2 | pending | coding | claude | M2 Phase E; deps task11, task12 |
| task14: Write `docs/specs/terminal-events.md` | AC-8.2 | pending | analyze | codex | M3 Phase A; deps task1 |
| task15: portable-pty Tokio actor with 1MB ring + UTF-8/ANSI-safe slicing + cancellation + TerminalEvent stream | AC-4.1, AC-4.2 | pending | coding | claude | M3 Phase B; deps task10, task14 |
| task16: xterm.js multi-tab frontend; ≥4 concurrent PTY stress test | AC-4.1 | pending | coding | claude | M3 Phase C; deps task15 |
| task17: Workspace storage layer (`workspaces.json` registry + canonical-path resolution + open-tab uniqueness + first-launch dir creation) | AC-4.3, AC-4.4, AC-9.1 | pending | coding | claude | M3 Phase D; deps task5 |
| task18: Workspace switcher modal (`+` button; create + pick-existing; per-row metadata from `.claude/`; modal coordinator) | AC-4.5 | pending | coding | claude | M3 Phase E; deps task17, task7 |
| task19: Open in Cursor + Reveal in Finder per workspace; configurable IDE; typed error toast | AC-4.6 | pending | coding | claude | M3 Phase F; deps task18 |
| task20: Orchestrator host-side tab (top-left fixed, single-instance lock, auto-launch claude) | AC-3.1, AC-3.2 | pending | coding | claude | M4 Phase A; deps task13, task16 |
| task21: Cross-tab read privileged capability path (`cross_tab_read` flag in Rust state only; bounded read API) | AC-3.3 | pending | coding | claude | M4 Phase B; deps task20, task15 |
| task22: Notification surface (notification + positioner TrayBottomCenter + dedup arbiter + lazy permission request + denial banner) | AC-8.1, AC-8.2, AC-8.4 | pending | coding | claude | M4 Phase D; deps task14, task20 |
| task23: Orchestrator semantic-summary notification path (OSC / MCP notification → tray entry) | AC-3.5, AC-8.3 | pending | coding | claude | M4 Phase E; deps task22, task20 |
| task24: Write `docs/specs/confirm-on-write.md` | AC-7.1, AC-7.5 | pending | analyze | codex | M5 Phase A; deps task1 |
| task25: Global confirm-on-write queue arbiter (single-modal-at-a-time + operation ID idempotency + per-tab pending indicator) | AC-7.1, AC-7.2, AC-7.3 | pending | coding | claude | M5 Phase B; deps task24, task4 |
| task26: Terminal stdin confirm-on-write wiring (pause stdin without blocking output/resize/exit) | AC-7.4 | pending | coding | claude | M5 Phase C; deps task25, task15 |
| task27: Write `docs/specs/gmail-sync.md` | AC-5.1, AC-5.2, AC-5.3, AC-5.4 | pending | analyze | codex | M6 Phase A; deps task6 |
| task28: Gmail sidecar scaffold (MCP server skeleton + OAuth loopback + browser-launch failure UX + duplicate-account detection) | AC-5.1, AC-2.1 | pending | coding | claude | M6 Phase B; deps task27, task11 |
| task29: Multi-account Stronghold storage + interrupted-setup recovery + orphan partial-OAuth cleanup | AC-5.2, AC-5.4 | pending | coding | claude | M6 Phase C; deps task28, task6 |
| task30: Gmail incremental sync engine (historyId, historyNotFound resync, 429 exponential backoff with 5-min ceiling) | AC-5.3 | pending | coding | claude | M6 Phase D; deps task29, task5 |
| task31: Gmail send via confirm-on-write with idempotent operation IDs | AC-5.5, AC-7.3 | pending | coding | claude | M6 Phase E; deps task30, task25 |
| task32: Gmail modify ops (label/archive/mark-read/unread/trash) via confirm-on-write; verify `gmail.delete` NOT registered | AC-5.6, AC-7.1 | pending | coding | claude | M6 Phase F; deps task30, task25 |
| task33: Gmail inbox UI (Chinese) + account switcher + attachment list/open-on-demand with 60s cleanup; NO compose/send UI buttons | AC-5.1, AC-5.7 | pending | coding | claude | M6 Phase G; deps task31, task32 |
| task34: Papers sidecar scaffold (MCP server skeleton) | AC-2.1 | pending | coding | claude | M7 Phase A; deps task11 |
| task35: Zotero three-mode access client (Live HTTP → SQLite-ro → cached snapshot; default + user-overridable path; status indicator) | AC-6.1 | pending | coding | claude | M7 Phase B; deps task34, task5 |
| task36: arxiv-rs query engine + DOI/arXiv-ID dedup + rule-based recommendations + polite caching | AC-6.2, AC-6.3, AC-6.4 | pending | coding | claude | M7 Phase C; deps task35 |
| task37: Papers tab UI (Chinese) with merged list + dedup badges + offline indicator | AC-6.1, AC-6.2 | pending | coding | claude | M7 Phase D; deps task36 |
| task38: AppQuitCoordinator (sequenced cleanup + startup stale-process reaping for force-quit recovery) | AC-9.4 | pending | coding | claude | M8 Phase B; deps task11, task15, task25 |
| task39: Orchestrator Gmail OAuth delegation pathway (orchestrator has no Gmail secrets; `gmail.send` flows through Gmail sidecar MCP + confirm-on-write + Stronghold token use, with audit tests for no token leakage) | AC-3.4 | pending | coding | claude | Added by Round 0 review; deps task20, task25, task31 |
| task40: Runtime scope-honesty documentation in orchestrator onboarding card and README (plugin-mediated writes are gated; raw workspace filesystem writes are not; force-quit cleanup limits are not overstated) | AC-7.5, AC-9.4 | pending | coding | claude | Added by Round 0 review; deps task13, task25, task38 |

### Blocking Side Issues
<!-- Only issues that directly block current mainline progress belong here -->
| Issue | Discovered Round | Blocking AC | Resolution Path |
|-------|-----------------|-------------|-----------------|

### Queued Side Issues
<!-- Non-blocking issues stay queued and must NOT replace the round objective -->
| Issue | Discovered Round | Why Not Blocking | Revisit Trigger |
|-------|-----------------|------------------|-----------------|
| Tracked `README.md` links point into `.humanize/`, but `.humanize/` is ignored and untracked by current hook policy | 0 review | Does not block local RLCR execution because the loop reads local `.humanize` files directly | Before sharing the repository or cutting a public/dev handoff, either move durable plan references under tracked docs or update README wording to mark them local-only |

### Completed and Verified
<!-- Only move tasks here after Codex verification -->
| AC | Task | Completed Round | Verified Round | Evidence |
|----|------|-----------------|----------------|----------|

### Explicitly Deferred
<!-- Items here require strong justification -->
| Task | Original AC | Deferred Since | Justification | When to Reconsider |
|------|-------------|----------------|---------------|-------------------|
