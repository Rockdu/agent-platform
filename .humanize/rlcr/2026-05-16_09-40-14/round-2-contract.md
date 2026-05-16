# Round 2 Contract (Drift Recovery)

## Root Cause of Drift

Round 0 and Round 1 both stalled because no source code was produced. The proximate cause: **the host machine has neither a Rust toolchain (`cargo`, `rustc` absent at all probed locations) nor a Node toolchain (`node`, `npm` absent at all probed locations).** The user has reported installing node ("我加了") but the binary is not findable at any of `/opt/homebrew/bin`, `/usr/local/bin`, `~/.nvm`, `~/.fnm`, `~/.volta`, `~/.asdf`. Cargo and rustc were also probed and found missing.

Without both toolchains, `task1` (Tauri v2 + React/TS scaffold + first-run bootstrap) is technically impossible to land: the React/Vite/TS portion needs `npm install`, and the Tauri/Rust portion needs `cargo check`/`cargo build`. The Codex round-1 review's "Required Implementation Plan" Step 1 explicitly demands resolving the Node blocker first, which has not been resolved.

## Recovered Mainline Objective (single)

**Produce the five plan-derived spec documents that have NO toolchain dependency, via the Codex `analyze` route, while task1 remains blocked on toolchain:**

- `docs/specs/plugin-contract.md` (task2 → AC-1.1, AC-1.4, AC-1.5)
- `docs/specs/mcp-sidecar.md` (task9 → AC-2.1, AC-1.6)
- `docs/specs/claude-launch.md` (task12 → AC-2.3, AC-3.2)
- `docs/specs/terminal-events.md` (task14 → AC-8.2)
- `docs/specs/confirm-on-write.md` (task24 → AC-7.1, AC-7.5)

These five tasks are **explicit `analyze` tasks in plan.md's Task Breakdown table** — each named with its target AC, owner = `codex`. They are deliverables the plan calls for in their own right, not workarounds. They also serve as the design substrate for the downstream `coding` tasks (task3 codegen, task10 stdio helper, task13 claude wiring, task15 PTY actor, task25 confirm-on-write queue) and de-risk those rounds. (task27 gmail-sync spec is omitted from this round because it depends on task6 Stronghold integration, which is itself part of task1's dependency chain; running it now would produce a spec without the implementation context.)

## Target ACs This Round (1–2)

This round's mainline targets the *spec deliverables* for these ACs, NOT their implementation:

- **AC-1.1 / AC-1.4 / AC-1.5** (plugin contract — manifest schema, mandatory permission gating, branded-handle `PluginCapability`): satisfied at the spec level by `docs/specs/plugin-contract.md`.
- **AC-2.1 / AC-1.6** (MCP stdio discipline + sidecar identity): satisfied at the spec level by `docs/specs/mcp-sidecar.md`.

The other ACs (AC-2.3, AC-3.2, AC-8.2, AC-7.1, AC-7.5) get their spec components in the same round but the contract's "1–2 ACs" framing refers to the AC SURFACES where this round produces deliverable evidence; all 5 specs land.

Implementation ACs (AC-9.1 etc.) remain pending. This round does NOT claim implementation progress — only spec progress.

## Blocking Issues (gate the recovered objective)

1. **Codex availability in stop-hook context.** Resolved in Round 1 via `.claude/settings.local.json` `env.PATH` injection. Verified working in this round's Bash invocations with explicit `export PATH="/opt/homebrew/bin:$PATH"`. The stop hook should also see codex once the env block is loaded at hook invocation.
2. **`ask-codex.sh` requires non-empty git repo + `CLAUDE_PROJECT_DIR` env var.** Both satisfied (git has commits; env var set in settings.local.json).

## Queued (Explicitly Out of Scope for Round 2)

1. **task1 implementation (Tauri scaffold + bootstrap + tab strip)** — blocked on missing Rust + Node toolchain. Round 3 must address this; either user provides paths to existing installs OR agent receives explicit permission to install Rust (`rustup`) and Node (brew/curl) directly.
2. **task3–task8, task10–task11, task13, task15–task38, task39, task40** — downstream `coding` tasks that need task1's substrate. All remain pending.
3. **task27 (gmail-sync spec)** — depends on task6 (Stronghold integration), which depends on task1. Spec without implementation context would be lower fidelity than running it after Stronghold lands.
4. **README.md docs/links rewrite** — the queued side issue that README points into ignored `.humanize/`. Non-blocking. Defer.
5. **Contract wording cleanup re AC-6.1** (queued by Codex round-1 review) — already fixed in tracker; no need to revisit contract files retroactively.

## Concrete Success Criteria (would change verdict to ADVANCED)

This round is COMPLETE when ALL of these hold:

1. **Five new spec files exist on disk under `docs/specs/`**: `plugin-contract.md`, `mcp-sidecar.md`, `claude-launch.md`, `terminal-events.md`, `confirm-on-write.md`. Each authored via `ask-codex.sh` with a structured prompt naming the target ACs and capturing the plan's Locked Decisions.
2. **Each spec is non-trivial** (target: ≥80 lines of substantive content, not a placeholder), covering: schema/types, lifecycle, error model, edge cases, and any normative DOs/DON'Ts that downstream coding tasks must respect.
3. **Each spec is referenced from `plan.md`'s "Specification Deliverables" section** (already present) — round 2 does not need to modify `plan.md`.
4. **Specs committed to git** in a single commit (the `docs/specs/` folder IS tracked; not under `.humanize/`).
5. **`goal-tracker.md` MUTABLE section updated**: task2/9/12/14/24 status → `completed (pending Codex verification)`; task1 status remains `pending` with the toolchain blocker preserved in Blocking Side Issues.
6. **`round-2-summary.md` documents what was produced** + the codex prompts used + a clear pointer to the next-round mainline (task1 once toolchain resolves).
7. **`bitlesson.md` updated** if any cross-round lesson emerges (e.g., "When upstream toolchain blocks a coding task, deliver downstream `analyze` deliverables that don't share the same blocker"). Otherwise `Action: none` is acceptable.

## What This Round Does NOT Try To Do

- It does NOT attempt to scaffold Tauri or React without toolchain (would produce code with no validation = same as Round 1's failure mode).
- It does NOT self-install Rust or Node without explicit user permission (that's a substantial side effect; round 3 starts there once user OKs).
- It does NOT modify the IMMUTABLE goal-tracker section.
- It does NOT broaden scope to "compensate for previous stalls" — the 5 specs are plan items, not invented work.

## Loop Posture Going Forward

If Codex round-2 review still demands task1 implementation despite spec deliverables, the appropriate Round 3 response is:
1. Resolve toolchain (install or get paths).
2. Implement task1 in full.
3. Mark the 5 specs as upstream context for codegen (task3) and dispatcher (task4) work.

The 5 specs are NOT throwaway — they directly constrain the implementation Codex demands and will accelerate task3+ rounds.
