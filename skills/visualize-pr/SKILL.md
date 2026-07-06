---
name: visualize-pr
description: >-
  Visualize a pull request — its diff, files, review state, checks, and
  discussion — as a self-contained, visual-first HTML page (files-changed
  table with add/del bars, review/check status board, diff-size donut,
  timeline of events, stacked-PR dependency graph). Use when the user wants to
  see the shape and status of a PR, what it touches, whether it's ready to
  merge, or how a stack of PRs relates. Reads `gh pr view`/`gh pr diff` and
  git. Pairs with the visualize-anything router and its shared engine.
---

# visualize-pr

A pull request on one page. Follow the visualize-anything shell + engine; this
skill supplies the `gh`/git queries and the figure recipe.

## 1. Gather data (`gh` CLI, JSON mode)

```bash
gh pr view <n> --json number,title,state,author,additions,deletions,\
changedFiles,files,reviews,statusCheckRollup,createdAt,mergedAt,\
labels,baseRefName,headRefName,commits,comments
gh pr diff <n> --name-only            # or parse files[] from above
```
`files[]` gives `{path, additions, deletions}`; `reviews[]` gives
`{author, state}`; `statusCheckRollup[]` gives `{name, conclusion}`. For a
**stacked PR** view, list the stack (`gh pr list --json number,title,baseRefName,state`)
and build edges base→head.

## 2. Figures by viewpoint (visual-first — see references/visual-first.md)

**👩‍💻 Developer (default)** — what changed & is it sound:
- `VIZ.kpis`: +additions / −deletions, files changed, commits, checks
  passing.
- `VIZ.table`: **files** — path(code), added(bar, green), removed(bar, red),
  status; sortable by size to see the bulk of the diff.
- `VIZ.donut`: diff composition by file type or by top directory.
- `VIZ.bars`: additions per top-level directory (where the change lands).
- `VIZ.table` (checks): name, conclusion(status), so failures are obvious.

**🙋 User / reviewer** — is it ready:
- One big state badge (open / draft / merged / closed) + `VIZ.kpis`
  (approvals, changes-requested, checks, age).
- `VIZ.timeline`: created → reviews → checks → merged, colored by outcome.
- `VIZ.donut`: reviews mix (approved / changes-requested / pending).

**🧭 Manager** — across a stack or a batch of PRs:
- `VIZ.kpis`: open PRs, avg age, % mergeable, checks red.
- `VIZ.graph` or `VIZ.dag`: **stacked-PR dependency** (base→head), node color
  = state — the Graphite-style stack view.
- `VIZ.table`: PRs ranked by age/size with status + bar cells.

## 3. Assemble & self-check

Build from `../visualize-anything/references/html-shell.md`, inline engine,
embed the `gh` JSON (trimmed) as `DATA`, run the visual-first self-check,
write `pr-<number>.html`, report the path.
