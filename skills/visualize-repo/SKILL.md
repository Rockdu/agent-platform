---
name: visualize-repo
description: >-
  Visualize a code repository — its structure, activity, contributors, and
  hotspots — as a self-contained, visual-first HTML page (commit-activity
  calendar heatmap, contributor bars, churn/hotspot table, language & file-type
  mix, directory treemap-style breakdown). Use when the user wants to see the
  shape or health of a repo, "where the work is", who changed what, or the
  busy/risky parts of a codebase. Reads git (log, shortlog, numstat) and
  optionally `gh`. Pairs with the visualize-anything router and its shared
  engine.
---

# visualize-repo

A repository on one page. Follow the visualize-anything shell + engine; this
skill supplies the git queries and the figure recipe.

## 1. Gather data (git — no script needed, these are enough)

Run from the repo root. Cap ranges so it stays fast (`--since=...`).
```bash
git -C <repo> log --date=short --pretty='%ad|%an' | sort | uniq -c   # commits/day, /author
git -C <repo> shortlog -sne --all | head -30                          # contributors
git -C <repo> log --since=90.days --numstat --pretty='%H'             # churn per file
git -C <repo> ls-files | sed 's/.*\.//' | sort | uniq -c | sort -rn   # file-type mix
git -C <repo> log --since=90.days --pretty='%ad' --date=format:'%u %H' # weekday×hour punch-card
```
For hotspots, combine change-frequency (commits touching a file) with size
(LOC) — "big + frequently changed = refactor risk" (the CodeScene heuristic).
Normalize the calendar/punch-card by the repo's own max, not a global scale.

## 2. Figures by viewpoint (visual-first — see references/visual-first.md)

**🧭 Manager (default)** — health & momentum:
- `VIZ.kpis`: commits (90d) + delta vs prior 90d (with `spark`), contributors,
  net LOC (+/−), open PRs (if `gh`), files touched.
- `VIZ.heat`: **commit calendar** (rows = weeks or weekdays, cols = days/hours),
  opacity by commit count — the single most "picture-not-words" repo figure.
- `VIZ.bars`: top contributors by commits.
- `VIZ.donut`: language / file-type mix (by file count or bytes — say which).

**👩‍💻 Developer** — structure & risk:
- `VIZ.table`: **hotspots** — file(code), commits(bar), LOC(num), lastChanged,
  status (red if big+churny). Sortable → refactor targets surface fast.
- `VIZ.bars`: churn by top-level directory (proxy for a treemap).
- `VIZ.heat`: file × month change matrix for the busiest files.
- `VIZ.tree`: directory tree with per-dir commit counts as meta (use `tree`
  with `children` = subdirs, `meta` = commit count).

**🙋 User / stakeholder** — "what's been happening":
- `VIZ.timeline`: milestones / release tags over time.
- `VIZ.kpis`: plain-language (features merged, active contributors, last
  release), `VIZ.donut` done vs in-progress if issue/PR data available.

## 3. Assemble & self-check

Build from `../visualize-anything/references/html-shell.md`, inline engine,
embed the parsed git data as `DATA`, run the visual-first self-check, write
`repo-<name>.html`, report the path.
