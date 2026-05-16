# Round 0 Contract

## Round Objective

**Initialization only.** Round 0 sets up the RLCR tracking infrastructure for the entire MVP implementation. No source code is written this round; the workspace remains a planning-artifacts-only repo at the end of R0.

## In-Scope (Round 0)

1. Populate the IMMUTABLE section of `goal-tracker.md` with:
   - Ultimate Goal (copied verbatim from the plan's Goal Description).
   - 9 main Acceptance Criteria (AC-1..AC-9 from the plan) — sub-ACs (AC-X.Y) remain documented in `plan.md` with their TDD positive/negative tests.
2. Populate the MUTABLE section's Active Tasks with all 38 tasks from `plan.md`, each tagged with target AC(s), routing tag (`coding`/`analyze`), and owner (`claude`/`codex`). All start status=pending.
3. Leave `bitlesson.md` at its empty template — no lessons exist yet.
4. Write this contract and the round-0 summary.
5. Commit the Round 0 state.

## Out-of-Scope (Round 0)

- No source code files (`src/`, `src-tauri/`, `plugins/`, etc.).
- No Cargo workspace scaffold.
- No package.json / npm install.
- No first task implementation. (Round 1 will begin with task1.)

## Round 0 Acceptance

This round is complete when:
- `goal-tracker.md` IMMUTABLE section is fully populated.
- All 38 tasks are listed in Active Tasks with correct AC mapping, tag, and owner.
- `round-0-summary.md` is written.
- Round 0 changes are committed.

## What Round 1 Will Look Like

Round 1 will attempt the **entire 38-task plan** (per RLCR semantics: "one round = the agent believes the plan is finished"). The actual execution will be heavy:
- ~32 coding tasks + 6 spec-writing (`analyze`) tasks routed to Codex.
- Scaffold + 8 milestones (Plugin Contract Foundation → MCP Sidecar → Terminal Mesh + Workspaces → Orchestrator → Confirm-on-Write → Gmail → Papers → Robustness).
- Likely many hours of autonomous work + many Codex consultations.

Once Round 1 attempts to complete, Codex reviews the round summary; if not COMPLETE, feedback drives Round 2+. Max iterations = 42 per state.md.

## Note on Plan Tracking

The plan file (`.humanize/plans/plan.md`) is tracked in git per `--track-plan-file`. Plan evolutions during implementation rounds must use the `Plan Evolution Log` table in `goal-tracker.md` MUTABLE section.
