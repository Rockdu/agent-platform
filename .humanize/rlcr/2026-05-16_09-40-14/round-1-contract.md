# Round 1 Contract

## Mainline Objective

**Implement `task1`: scaffold the Tauri v2 + React/TypeScript host shell with the fixed-orchestrator-slot tab strip and first-run directory bootstrap.**

This is a single, narrowly-scoped objective. Per `plan.md` task table, `task1` is the substrate that all subsequent plugin/contract/MCP/sidecar work depends on; it must land first.

## Target ACs (1–2)

- **AC-9.1** (primary): First-run bootstrap creates `~/AgentPlatform/`, `~/AgentPlatform/workspaces/`, `${APP_DATA}/plugins/`, `${APP_DATA}/logs/`, `${APP_DATA}/claude-mcp-configs/`. Permission failures surface a user-readable error.
- **AC-6.1** (partial): The three MVP tabs render on first launch with empty/placeholder state — this round establishes the empty 3-tab strip (orchestrator slot leftmost fixed + 3 plugin placeholders). Plugin failure isolation (the other half of AC-6.1) requires the plugin contract pipeline (task3+) and is OUT OF SCOPE for this round; this round only verifies the shell renders three placeholder tabs.

## Blocking Issues (gate the mainline objective)

1. **Toolchain unavailable**: `node` and `npm` are not in the Claude Code Bash tool's PATH. The user reports they installed node but the binary is not at any standard path I have probed (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.nvm`, `~/.fnm`, `~/.volta`, `~/.asdf`). Awaiting user input on the correct path so it can be added to `.claude/settings.local.json` `env.PATH`. Without node/npm, the React/TS scaffold cannot be initialized.
2. **`.humanize/` re-tracking from external commit**: a sibling window committed `recover .humanize` (`5833b91`), re-tracking the humanize folder against plugin hook policy. Resolved in commit `92dfc46` (restore `.gitignore`, re-untrack). Defensive note: if external recovery happens again mid-round, re-execute the same untrack pattern; do not let it block the mainline objective.

## Queued (Explicitly Out of Scope for Round 1)

Everything else from the v2 plan's 40-task list. Specifically (each carries its own milestone phase per plan.md):

- task2 (plugin-contract.md spec) — depends on task1
- task3–task8 (plugin contract pipeline: codegen, dispatcher, SQLite framework, Stronghold, lifecycle hooks, dev diagnostics)
- task9–task13 (MCP sidecar discipline + claude wiring)
- task14–task19 (Terminal Mesh + workspaces)
- task20–task23 (Orchestrator + notification surface)
- task24–task26 (Confirm-on-write queue)
- task27–task33 (Gmail plugin end-to-end)
- task34–task37 (Papers plugin end-to-end)
- task38 (AppQuitCoordinator)
- task39 (Orchestrator Gmail OAuth delegation; added by R0 review)
- task40 (Runtime scope-honesty docs; added by R0 review)

The R0 review's "Required Implementation Plan For Next Round" (10 numbered phases) maps to roughly 8 future rounds, not this one. Round 1 deliberately stays tight to ship the substrate first; subsequent rounds will tackle Plugin Contract Foundation (M1 remaining phases), MCP wiring, etc.

## Concrete Success Criteria for Round 1

This round completes when ALL of the following hold:

1. **Repo structure exists**: `src-tauri/` (Tauri binary crate with `Cargo.toml`, `src/main.rs`, `tauri.conf.json`, `build.rs` placeholder) and `src/` (React/TS frontend with `package.json`, `index.html`, `vite.config.ts`, `tsconfig.json`, `src/main.tsx`, `src/App.tsx`).
2. **`cargo check --manifest-path src-tauri/Cargo.toml` passes** (Rust compiles, even if it is just a stub `tauri::Builder::default().run(...)`).
3. **`npm install` succeeds** and `npm run build` (or equivalent Vite build) produces a `dist/` (or configured frontend bundle output).
4. **First-run bootstrap implemented in Rust** at `src-tauri/src/bootstrap.rs` (or inside `main.rs`) that creates the 5 required directories on first launch; calls into it from `tauri::Builder::default().setup(...)` hook. Permission-failure path returns a typed error and renders a Chinese error card in the frontend.
5. **Empty 3-tab strip renders in the React shell**: a left-fixed Orchestrator slot (distinct icon placeholder) + three plugin slot placeholders (Terminal Mesh / Gmail / Papers). Tabs are non-functional empty cards; no plugin loading yet.
6. **Project builds without runtime panic**: the result of `cargo build --manifest-path src-tauri/Cargo.toml` (or `tauri build` if available) does not exit non-zero. (Actually running the GUI is out of scope — it requires a window-server session which is unreliable in the loop context.)
7. **Round 1 changes committed** in a single commit (excluding `.humanize/`, per hook policy). Commit subject mentions `task1` and the AC mapping.
8. **`goal-tracker.md` MUTABLE section updated**: task1 status moved to `completed` (pending Codex verification); a `Plan Evolution Log` entry references this round if any plan deviation was needed.
9. **`bitlesson.md`** — `Action: none` is acceptable for Round 1 if no novel cross-round lesson surfaces. If toolchain misadventures generate a lesson (e.g., "Claude Code Bash tool does not inherit shell PATH; node tools must be reachable via `.claude/settings.local.json` `env.PATH`"), add it.
10. **`round-1-summary.md`** documents what was implemented + files changed + verification commands + BitLesson delta.

## Out-of-Scope but Worth Flagging in This Contract

- The R0 review explicitly demands "no task marked complete without implementation evidence + verification command/test result". Round 1's task1 will have: `cargo check` output, `npm run build` output, manual inspection of file tree, and a screenshot or AST-level confirmation of the 3-tab placeholder render (deferring full UI screenshot to when the dev server can be started safely).
- The R0 review correctly notes that `README.md` currently has links into the (now-untracked) `.humanize/` directory. This is `[queued]` and should be revisited when the project is shared. For Round 1 we leave README.md alone.
- Several tasks in the plan are tagged `analyze` (Codex specs): task2, task9, task12, task14, task24, task27. Round 1 does NOT execute any of these — task1 is `coding` and self-contained.

## How Round 1 Should Be Verified by Codex

The Codex round-1 review should look for:
- `src-tauri/Cargo.toml`, `src-tauri/src/main.rs`, `src-tauri/tauri.conf.json` exist and are minimally valid.
- `package.json` declares React + TypeScript + Vite + `@tauri-apps/api` (or equivalent) dependencies.
- `src/App.tsx` contains a tab strip with exactly 4 slots: 1 Orchestrator (fixed-leftmost, distinct visual treatment) + 3 plugin placeholders.
- A `bootstrap` module in Rust creates the 5 specified directories on first run with a typed error path for permission failures.
- The build output proves Round 1 actually built (not just file scaffolding).
- `goal-tracker.md` reflects task1 status correctly.

Anything not on this list (plugin manifests, MCP servers, dispatchers, PluginCapability, etc.) is **NOT a Round 1 failure**; those land in subsequent rounds per the milestone sequence.
