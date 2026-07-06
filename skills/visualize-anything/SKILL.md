---
name: visualize-anything
description: >-
  Router + shared engine for turning almost anything into a self-contained,
  visual-first HTML page (charts, relationship graphs, DAGs, timelines,
  tables — minimal prose). Use whenever the user asks to "visualize",
  "可视化", diagram, chart, map out, or show the state/structure/history of:
  a code repository, a pull request, the agent platform's agents and their
  relationships & runtime status, or a single agent's history and its
  processing chain (tool-call trace). Picks a viewpoint (developer / user /
  manager) and dispatches to the right sub-skill. Invoke this first when the
  target category is unclear.
---

# visualize-anything

Turn a target into **one self-contained HTML file** that is *visual-first*:
charts, relationship graphs, DAGs, timelines, and tables carry the meaning;
prose is the exception. This skill is the **router + shared rendering
engine**. It (1) picks the viewpoint, (2) picks the category sub-skill, and
(3) hands both the shared assets so every output looks like one system.

## The hard rule: visual-first (non-negotiable)

> **Every generated page MUST contain more visual elements (charts, graphs,
> DAGs, timelines, tables, KPI tiles, status badges) than text blocks.**
> Kill prose. A short caption per figure and a one-line takeaway are fine;
> paragraphs are not. If you cannot express something as a figure or table,
> ask whether it belongs on the page at all.

Before returning, run the self-check in `references/visual-first.md`. If the
count of `<figure>/table/svg/kpi` blocks does not exceed the count of text
blocks, convert more prose into tables/badges/small-multiples until it does.

## Two axes: viewpoint × category

Every request resolves to a **(viewpoint, category)** pair.

**Viewpoint** — who is looking (full definitions in `references/viewpoints.md`):
| Lens | Cares about | Leads with |
|---|---|---|
| 👩‍💻 Developer | structure, correctness, mechanics, failures | DAGs, traces, diffs, file/tool tables |
| 🙋 User | outcome, progress, what happened, what's next | timelines, status badges, plain-language KPIs |
| 🧭 Manager | throughput, health, cost, trends, risk | KPI tiles, distributions, heatmaps, rollup tables |

If the user does not name a viewpoint, infer from their wording; when still
ambiguous, default to **developer** and add a small lens switcher note. You
may render **multiple lenses as tabs/sections** in one page when asked to
"cover all viewpoints".

**Category** → dispatch to a sub-skill:
| Target | Sub-skill | Reads |
|---|---|---|
| A code repository (structure, activity, contributors, hotspots) | `visualize-repo` | git / `gh` |
| A pull request (diff, review state, files, discussion) | `visualize-pr` | `gh pr`, git |
| The platform's agents — relationships & runtime status | `visualize-agents` | `~/.claude/projects/**`, platform runtime state |
| A single agent's history & processing chain (tool trace) | `visualize-agent-trace` | one session `.jsonl` |

If the target does not fit these, still apply this skill's engine + viewpoint
model and build a bespoke page from the primitives — the four sub-skills are
worked examples, not a closed set ("visualize **anything**").

## Workflow

1. **Resolve (viewpoint, category)** from the request. State your choice in
   one line to the user.
2. **Read the sub-skill** for the category (`skills/<name>/SKILL.md`) — it
   defines what data to gather, the exact data shapes, and which figures to
   render for each viewpoint.
3. **Gather data** using that sub-skill's collector script or commands.
   Prefer the provided scripts; they emit normalized JSON.
4. **Build the page** from the shared shell + engine (see below). Embed the
   data as an inline `<script>` JSON literal so the file is fully portable.
5. **Self-check** against `references/visual-first.md`, then write the `.html`
   to the workspace and tell the user the path. Offer to publish it as an
   Artifact if they want a shareable link.

## Shared engine (use it — do not hand-roll charts)

Two inline assets make every page consistent, dependency-free, theme-aware,
and Artifact-safe (no CDN / no external fetch):

- `assets/viz.css` — palette + layout. Inline verbatim into `<head>`.
- `assets/viz.js` — renderers on `window.VIZ`. Inline verbatim before `</body>`.

`VIZ` API (all take an element or selector as first arg):
`VIZ.kpis`, `VIZ.table`, `VIZ.bars`, `VIZ.donut`, `VIZ.dag` (layered flow),
`VIZ.graph` (radial relationships, may cycle), `VIZ.timeline` (gantt),
`VIZ.heat` (heatmap). Signatures and data shapes are documented at the top of
`assets/viz.js` and in `references/html-shell.md`, which also contains a
copy-paste page skeleton. **Build every page from this skeleton.**

Shared data shapes (session-log and runtime-state schemas, reused across
sub-skills) live in `references/data-contracts.md`.

## References
- `references/viewpoints.md` — developer / user / manager lens definitions + figure menus.
- `references/visual-first.md` — the mandate, the self-check, prose→visual conversions.
- `references/chart-chooser.md` — pick the right figure by data relationship; encoding-honesty, accessibility, and rendering-budget rules (distilled from LangSmith/Langfuse/Phoenix, GitHub/CodeScene, Tufte/Few/FT).
- `references/html-shell.md` — the page skeleton + how to call each `VIZ.*` renderer.
- `references/data-contracts.md` — session `.jsonl` + platform runtime-state schemas.

`VIZ` renderers: `kpis` (tiles, optional `spark`), `table`, `bars`, `donut`,
`dag` (layered flow), `graph` (radial, may cycle), `timeline` (gantt), `heat`
(matrix / calendar), `tree` (collapsible `<details>` chain/hierarchy),
`agentmap` (labeled relationship map of rounded-rect cards, each with an
incisive "doing" one-liner and expand-in-place to a condensed trace — the
primary view for "how agents relate + what each is doing").

**North star:** convey the object and its information to the user *as fast as
possible*; cut anything that slows comprehension. Lead with the single most
informative view and use progressive disclosure for detail.
