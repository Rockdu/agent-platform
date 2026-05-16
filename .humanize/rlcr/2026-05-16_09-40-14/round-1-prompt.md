Your work is not finished. Read and execute the below with ultrathink.

## Original Implementation Plan

**IMPORTANT**: Before proceeding, review the original plan you are implementing:
@.humanize/plans/plan.md

This plan contains the full scope of work and requirements. Ensure your work aligns with this plan.

---

## Round Re-anchor (REQUIRED FIRST STEP)

Before writing code:
- Re-read @.humanize/plans/plan.md
- Re-read @/Users/rockdu/claude_workspace/.humanize/rlcr/2026-05-16_09-40-14/goal-tracker.md
- Re-read the most recent round summaries/reviews that led to this round
- Write the current round contract to @/Users/rockdu/claude_workspace/.humanize/rlcr/2026-05-16_09-40-14/round-1-contract.md

Your round contract must contain:
- Exactly one **mainline objective**
- The 1-2 target ACs for this round
- Which issues are truly **blocking** that mainline objective
- Which issues are **queued** and explicitly out of scope
- Concrete success criteria for this round

Do not start implementation until the round contract exists.

## Task Lane Rules

Use the Task system (TaskCreate, TaskUpdate, TaskList) with one required tag per task:
- `[mainline]` for plan-derived work that directly advances this round's objective
- `[blocking]` for issues that prevent the mainline objective from succeeding safely
- `[queued]` for non-blocking bugs, cleanup, or follow-up work

Rules:
- `[mainline]` work is the round's primary success condition
- `[blocking]` work is allowed only when it truly blocks the mainline objective
- `[queued]` work must be documented but must NOT replace the round objective
- If a new bug does not block the current objective, tag it `[queued]` and keep moving on mainline work

Before executing each task in this round:
1. Read @/Users/rockdu/claude_workspace/.humanize/bitlesson.md
2. Run `bitlesson-selector` for each task/sub-task
3. Follow selected lesson IDs (or `NONE`) during implementation

---
Below is Codex's review result:
<!-- CODEX's REVIEW RESULT START -->
# Round 0 Review Result

Mainline Progress Verdict: STALLED

## Summary

Claude completed most of the Round 0 tracking setup, but no MVP implementation exists yet. There is no Tauri scaffold, no Rust code, no React/TypeScript frontend, no plugin sidecars, no MCP wiring, no tests, and no acceptance criterion can be considered implemented.

The "initialization-only" scope explains why no source code was written, but under the review contract this is not COMPLETE: all original plan tasks remain pending, and two acceptance-criterion requirements were under-tracked in the active task list until this review corrected the tracker.

## Goal Alignment Summary

ACs: 0/9 addressed | Forgotten items: 2 detected and tracker-corrected | Unjustified deferrals: 0

- AC-1 through AC-9 are tracked in the immutable section, but none has implementation progress.
- All 38 task-table items from `plan.md` were initially listed as pending.
- Review found missing direct active-task coverage for AC-3.4 and runtime documentation coverage for AC-7.5/AC-9.4, then added `task39` and `task40` to the mutable tracker.
- No task is completed, verified, or explicitly deferred.

## Tracker Updates Performed

Updated `.humanize/rlcr/2026-05-16_09-40-14/goal-tracker.md` mutable section only:

- Bumped plan version to 2 for Round 0 review.
- Added a Plan Evolution Log entry explaining the tracker correction.
- Added `task39`: Orchestrator Gmail OAuth delegation pathway for AC-3.4.
- Added `task40`: Runtime scope-honesty documentation in orchestrator onboarding card and README for AC-7.5 and AC-9.4.
- Added a queued side issue noting that tracked `README.md` links point into ignored/untracked `.humanize/` local files.

## Mainline Gaps

1. **All MVP implementation work remains unfinished.**
   Evidence: repository currently has only `README.md` tracked as application content; there is no `src/`, `src-tauri/`, `plugins/`, `docs/specs/`, `package.json`, `Cargo.toml`, sidecar crate, frontend, or test suite. The Active Tasks table still has every implementation/spec task pending.

2. **No acceptance criterion has executable evidence.**
   The tracker summarizes AC-1..AC-9, but there are no positive or negative tests for manifest validation, IPC codegen, permission gating, MCP stdio framing, `claude` launch, PTY behavior, Gmail OAuth/sync, Papers/Zotero/arXiv, confirm-on-write, notifications, or quit sequencing.

3. **AC-3.4 was not directly represented in the original Active Tasks table.**
   The plan explicitly requires orchestrator Gmail send via OAuth delegation with no token crossing orchestrator, `claude` stdio, config files, env vars, or logs. Existing `task31` covered Gmail send/idempotency, but did not target AC-3.4 or require the orchestrator-specific no-secret/no-leak audit path. This is now tracked as `task39`.

4. **AC-7.5 runtime documentation was under-tracked.**
   `task24` only writes `docs/specs/confirm-on-write.md`, but AC-7.5 requires the scope boundary to be documented in the orchestrator onboarding card and project README. AC-9.4 also requires docs/onboarding not to overclaim force-quit cleanup. This is now tracked as `task40`.

5. **Round 0 contract says Round 0 changes are committed, but `.humanize/` is ignored and untracked.**
   Current `git ls-files` only shows `README.md` and `.gitignore`; `git status --ignored .humanize` reports `.humanize/` ignored. This may be intentional hook policy, but it contradicts the Round 0 contract and Claude's completion criteria unless the contract/summary explicitly states that RLCR artifacts are local-only and not commit targets.

## Blocking Side Issues

None beyond the mainline gaps above. The next round should not spend time on unrelated cleanup; it must implement the MVP tasks.

## Queued Side Issues

1. **README links point into ignored local `.humanize/` files.**
   This does not block local RLCR execution, but it will confuse any checkout or handoff where `.humanize/` is absent. Before sharing the repository, either move durable plan references under tracked `docs/` or label those links as local-only.

## Required Implementation Plan For Next Round

Claude must execute the implementation plan, not another initialization-only pass:

1. Build the Tauri v2 + React/TypeScript shell first (`task1`), including first-run directory bootstrap and the fixed orchestrator slot.
2. Produce the six required specs through Codex-routed analyze tasks at their dependency points: plugin contract, MCP sidecar, Claude launch, terminal events, confirm-on-write, and Gmail sync.
3. Implement the plugin contract pipeline before plugin feature work: manifest schema/uniqueness checks, generated Rust + TS registries, typed IPC wrappers, permission metadata, dispatcher gating, `PluginCapability`, per-plugin SQLite, Stronghold, diagnostics, and capability lifecycle.
4. Implement MCP/sidecar foundations: stdio framing, log discipline, shutdown protocol, lifecycle manager, per-tab MCP config generation, `claude` PATH discovery, onboarding, and sidecar restart behavior.
5. Implement Terminal Mesh and workspace foundations: portable PTY actor with 1 MB bounded ring, UTF-8/ANSI-safe slicing, xterm.js multi-tab UI, workspace registry, canonical-path uniqueness, switcher modal, IDE handoff, and stress tests for at least four concurrent PTYs.
6. Implement orchestrator behavior: single-instance privileged host tab, auto-launch `claude`, cross-tab read capability, semantic-summary notification, and the newly tracked `task39` Gmail delegation path with no Gmail secrets reachable by orchestrator.
7. Implement confirm-on-write globally before Gmail write tools: one host-owned queue, idempotent operation IDs, pending indicators, terminal stdin approval that does not block PTY output/resize/exit, and explicit runtime scope-honesty documentation from `task40`.
8. Implement Gmail end to end: multi-account OAuth, Stronghold-only refresh tokens, interrupted setup recovery, incremental sync/history recovery/backoff, send/modify via confirm-on-write, attachment open-on-demand with cleanup, and Chinese inbox UI with no compose/send buttons.
9. Implement Papers end to end: MCP sidecar, Zotero live/SQLite-ro/cache fallback, arXiv query/dedup/recommendations/cache guard, and Chinese UI with status indicators.
10. Finish shell robustness: plugin-scoped migration failure UX, structured JSON logs with redaction and correlation IDs, notification surface, quit coordinator, startup stale-process reaping, and full positive/negative AC test coverage.

Do not mark any task complete until there is implementation evidence and a verification command or test result. The next review should expect all active tasks, including `task39` and `task40`, to be implemented or it will remain incomplete.

NOT COMPLETE
<!-- CODEX's REVIEW RESULT  END  -->
---

## Goal Tracker Reference

Before starting work, **read** @/Users/rockdu/claude_workspace/.humanize/rlcr/2026-05-16_09-40-14/goal-tracker.md to understand:
- The Ultimate Goal and Acceptance Criteria you're working toward
- Which tasks are Active, Completed, or Deferred
- Which side issues are blocking vs queued
- Any Plan Evolution that has occurred
- The latest side-issue state that needs attention

**IMPORTANT**: Keep the mutable section of `goal-tracker.md` up to date during the round.
Do NOT change the immutable section after Round 0.
If you cannot safely reconcile the tracker yourself, include an optional "Goal Tracker Update Request" section in your summary (see below).

## Mainline Guardrails

- Keep the mainline objective from @/Users/rockdu/claude_workspace/.humanize/rlcr/2026-05-16_09-40-14/round-1-contract.md stable for this round
- Do not let queued issues take over the round
- If Codex reported several findings, classify them into:
  - mainline gaps
  - blocking side issues
  - queued side issues
- Only mainline gaps and blocking side issues should drive the next code changes

---

Note: You MUST NOT try to exit by lying, editing loop state files, or executing `cancel-rlcr-loop`.

After completing the work, please:
0. If the `code-simplifier` plugin is installed, use it to review and optimize your code. Invoke via: `/code-simplifier`, `@agent-code-simplifier`, or `@code-simplifier:code-simplifier (agent)`
1. Commit your changes with a descriptive commit message
2. Write your work summary into @/Users/rockdu/claude_workspace/.humanize/rlcr/2026-05-16_09-40-14/round-1-summary.md

## Task Tag Routing Reminder

Follow the plan's per-task routing tags strictly:
- `coding` task -> Claude executes directly
- `analyze` task -> execute via `/humanize:ask-codex`, then integrate the result
- Keep Goal Tracker Active Tasks columns `Tag` and `Owner` aligned with execution

**Optional fallback**: if you could not safely update the mutable section of `goal-tracker.md` directly, include this section in your summary:
```markdown
## Goal Tracker Update Request

### Requested Changes:
- [E.g., "Mark Task X as completed with evidence: tests pass"]
- [E.g., "Add to Blocking Side Issues: bug Y blocks AC-2"]
- [E.g., "Add to Queued Side Issues: cleanup Z is non-blocking"]
- [E.g., "Plan Evolution: changed approach from A to B because..."]
- [E.g., "Defer Task Z because... (impact on AC: none/minimal)"]

### Justification:
[Explain why these changes are needed and how they serve the Ultimate Goal]
```

Codex will review your request and reconcile the Goal Tracker if justified.
