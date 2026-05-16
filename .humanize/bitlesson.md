# BitLesson Knowledge Base

This file is project-specific. Keep entries precise and reusable for future rounds.

## Entry Template (Strict)

Use this exact field order for every entry:

```markdown
## Lesson: <unique-id>
Lesson ID: <BL-YYYYMMDD-short-name>
Scope: <component/subsystem/files>
Problem Description: <specific failure mode with trigger conditions>
Root Cause: <direct technical cause>
Solution: <exact fix that resolved the problem>
Constraints: <limits, assumptions, non-goals>
Validation Evidence: <tests/commands/logs/PR evidence>
Source Rounds: <round numbers where problem appeared and was solved>
```

## Entries

## Lesson: claude-code-bash-path-isolation
Lesson ID: BL-20260516-claude-bash-path
Scope: dev environment, toolchain, hooks, setup scripts
Problem Description: Claude Code's Bash tool runs in a non-interactive shell that does NOT source `~/.zprofile` or `~/.zshrc`. As a result, PATH adjustments those files do (e.g., `eval "$(/opt/homebrew/bin/brew shellenv zsh)"`) are absent. Tools installed under `/opt/homebrew/bin` (Apple Silicon brew), `~/.nvm/...`, `~/.fnm/...`, etc. are not findable by `command -v` or by hooks invoked from the loop. The `loop-codex-stop-hook.sh` rejects exit when `codex` is not in PATH, even though the user did install it.
Root Cause: Login-shell init files are only sourced by login shells. The Bash tool's invocation pattern is non-interactive and non-login, so it sees only the inherited PATH from the harness process.
Solution: Add an `env` block to `.claude/settings.local.json` listing the full PATH explicitly: `{"env": {"PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/Users/rockdu/.local/bin"}}`. This applies both to the Bash tool and to hooks. Verified after applying the change: `command -v codex` resolves at next session. For nvm/fnm/Volta users, the per-tool node version path must be discovered (`which node` in the user's interactive shell) and added to this PATH.
Constraints: settings.local.json env changes take effect at next session/turn start, not retroactively in the current Bash session. Hooks read settings at runtime so they pick up the new PATH at next invocation.
Validation Evidence: Pre-fix: `which codex` returns "not found", stop-hook errors with "Codex CLI Not Found". Post-fix: stop-hook no longer fires that error. Next-session `which codex` resolves to `/opt/homebrew/bin/codex`. The same fix applies for any non-default-PATH tool (node, npm, cargo if installed outside /usr/bin, etc.).
Source Rounds: 1 (problem surfaced during task1 toolchain check)

## Lesson: codex-stdio-vs-filesystem-tools
Lesson ID: BL-20260516-codex-output-channel
Scope: any use of `ask-codex.sh` to produce spec docs or other markdown content
Problem Description: When `ask-codex.sh` is invoked with a prompt containing phrasing like "Saved verbatim to `path/to/file.md`", codex's CLI tools (which include filesystem write access by default with `--full-auto`/`--sandbox workspace-write`) interpret that phrasing as a directive to write the file itself. Codex then writes the spec content directly to the requested path AND returns only a short summary like `Saved to docs/specs/X.md` in stdout. If the caller then naively `cp .humanize/skill/<id>/output.md $dest`, the destination file is OVERWRITTEN with the short summary, clobbering the real content codex wrote. Symptom: docs/specs/terminal-events.md ended up at 12 lines (a "Saved to..." summary) when the prompt expected a multi-hundred-line spec.
Root Cause: Codex's CLI runs with workspace-write sandbox by default; its tool surface includes file write. Prompts that read like instructions to save the file are interpreted literally.
Solution: In codex prompts for spec/doc generation, explicitly instruct: "Output ONLY the markdown content in stdout. Do NOT use any filesystem write tool to save the file yourself — the caller will save your stdout to <path>. No 'Saved to...' summary." Test verification: stdout-length should match expected spec size (hundreds of lines), not a 5-12 line summary. Inverse pattern (let codex write directly, then `cat` it back) is brittle and not recommended; stdout is the safer authoritative channel.
Constraints: Applies to any `ask-codex.sh` invocation where the agent intends to control where output lands. Not relevant for codex calls that are purely analytical (no spec deliverable).
Validation Evidence: Round 2 of RLCR loop: tasks 2, 9, 12 had stdout with full content (1341, 422, 509 lines); tasks 14 and 24 returned 12 and 5 lines respectively because codex wrote files directly. Task 14 re-run with explicit "do not use filesystem write tool" produced 563 lines of stdout content as expected.
Source Rounds: 2

<!-- Add lessons below using the strict template. -->
