# Chart chooser + encoding rules (field-tested)

Distilled from how the leading tools do it — LLM-trace UIs (LangSmith,
Langfuse, Phoenix, Weave, AgentOps), repo viz (Gource, git-of-theseus,
GitHub Insights, CodeScene), and the dataviz canon (Tufte, Few, NN/g, FT
Visual Vocabulary). Use it to pick the *right* figure, not just a figure.

## Pick by data relationship (FT Visual Vocabulary, condensed)

| Your data is about… | Use | `VIZ.*` | Avoid |
|---|---|---|---|
| Ranking / "most X" | horizontal bars, table w/ bar cells | `bars`, `table` | pie |
| Part-to-whole (few parts) | donut / stacked bar | `donut` | donut w/ >5 slices |
| Change over time | timeline, line/spark | `timeline`, spark tile | — |
| Magnitude across categories | bars | `bars` | angle/area encodings |
| Distribution / density over 2D | heatmap, calendar/punch-card | `heat` | heatmap w/ <9 cells |
| Flow / sequence of steps | layered DAG, gantt | `dag`, `timeline` | Sankey w/ many nodes |
| Hierarchy / nesting / call tree | collapsible tree | `tree` | deep DAG (>60 nodes) |
| Relationships / topology (may cycle) | node-link graph | `graph` | tree (if cyclic) |
| Agents/entities that relate AND each need a "what it's doing" line + drill-in | labeled relationship map | `agentmap` | plain `graph` (no labels/detail) |
| One metric vs a target | KPI tile w/ delta | `kpis` | gauge |

## The four workhorse figures (cover ~80% of cases)

1. **Bar waterfall / gantt** (`timeline`) — "when + how long". One row per
   span; bar length = duration; color = status. This is the trace default.
2. **Collapsible span tree** (`tree`) — "what called what". Native
   `<details>`, zero JS, expands on click; each row shows label + `sub` +
   status badges + right-aligned meta (ms/tokens). Best for processing chains.
3. **Layered node-link DAG** (`dag` / `graph`) — "topology". `dag` for
   acyclic flows (left→right layered); `graph` for relationships with a hub.
4. **Calendar / punch-card heatmap** (`heat`) — "activity rhythm". rows ×
   cols grid, opacity ramp by count. Great for commits-by-day/hour.

## Encoding honesty (non-negotiable)

- **Length and position beat area and angle.** Prefer `bars`/`timeline` over
  `donut`; use `donut` only for ≤5 parts of a clear whole.
- **Normalize by the dataset's own range**, not a global scale (heatmaps,
  sparklines, calendar) — that's how GitHub's contribution graph reads well.
- **Every mark exposes magnitude + status + time**; exact numbers go in a
  native `<title>` tooltip so overview stays sparse (overview-first, detail
  on demand).
- **High data-ink, no chartjunk**: no 3D, no decorative gradients, no heavy
  gridlines. viz.css already enforces this — don't add ornament.

## Accessibility / color (redundant, not decorative)

- Color is **semantic** (status) or **categorical** (series) — never both on
  one page. Status = `--st-*`; series = `--cat-*`.
- **Never rely on color alone.** Status is always accompanied by its text
  (badge label) or a shape; the palette holds ≥3:1 non-text contrast and
  stays legible in grayscale and for the ~8% with color-vision deficiency.

## Rendering budget (self-contained HTML)

- Default stack: **inline SVG + CSS + vanilla JS** (what `VIZ` uses). No chart
  library, no CDN.
- Element count: SVG is fine up to ~1k marks. 1k–10k → switch to `<canvas>`.
  >10k → WebGL. Most pages never leave SVG; if a trace has 1k+ steps,
  aggregate (tool families, sub-agents) instead of drawing every mark.

## Artifacts CSP hard rules (also good hygiene for local files)

Inline **all** CSS/JS; images as `data:` URIs; **no** external host, `fetch`,
XHR, or WebSocket; single page (use in-page anchors, not relative links);
keep the rendered file well under 16 MiB.
