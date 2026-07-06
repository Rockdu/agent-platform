# Visual-first mandate + self-check

The one rule that overrides style preference: **visual elements must outnumber
text blocks on every page.** This file is how you comply.

## What counts

- **Visual element** = one `<figure>` from `VIZ.*`, one `<table>`, one
  standalone `<svg>`, one KPI tile row, or a row of status badges/chips.
- **Text block** = a `<p>`, a bullet list, or a multi-sentence caption. A
  ≤1-line figure caption or a single takeaway line does **not** count as a
  text block.

Target ratio: **visual : text ≥ 2 : 1**. Hard floor: strictly greater than 1:1.

## The self-check (run before writing the file)

1. Count visual elements `V` and text blocks `T`.
2. If `V <= T`, you have too much prose. Do the conversions below until
   `V > T` (aim for `V ≥ 2T`).
3. Verify every figure has a data source (no decorative-only charts) and a
   ≤6-word caption.
4. Verify the page renders with **zero external requests** (no CDN, no
   remote fonts/images). Everything inline.

## Prose → visual conversions (do these aggressively)

| If you were about to write… | Render instead |
|---|---|
| A paragraph describing steps | `VIZ.timeline` or `VIZ.dag` |
| "There are N X, of which…" | `VIZ.donut` or `VIZ.kpis` |
| A list of items with attributes | `VIZ.table` (typed cells) |
| "X is healthy / failing / busy" | status badge cell / KPI accent |
| "A calls B calls C" | `VIZ.dag` |
| "A relates to B, C, D" | `VIZ.graph` |
| A ranking ("most changed files") | `VIZ.bars` or table with `bar` cells |
| "activity was high on Tue/Wed" | `VIZ.heat` |
| A metric that moved | KPI tile with `delta` |
| A long id / command | `code` cell, truncated, full value in `title=` |

## Allowed text

- One `<h1>` + scope line in the header.
- A meta strip (source, timestamp, lens) — treat as chrome, not a text block.
- ≤1 caption per figure and an optional ≤1-line "takeaway" beside it.
- A short footer (data provenance, counts). 

Everything else earns its place as a figure or table, or it is cut.

## Density, not clutter

Visual-first ≠ noisy. Prefer **small multiples** and **one dense table** over
many tiny disconnected charts. Group figures with `VIZ.grid cols-2/3`. Keep a
single categorical palette across the page (`--cat-*`) and reserve status
colors (`--st-*`) for state only.
