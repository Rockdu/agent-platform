# Viewpoints — the three lenses

Same data, three audiences. Each lens changes **what leads the page**, **which
figures appear**, and **how much you aggregate**. Never just relabel — re-rank
and re-aggregate.

---

## 👩‍💻 Developer lens

**Question they're answering:** "How does it actually work, and where does it
break?"

- **Altitude:** low. Individual tool calls, files, commits, spans, errors.
- **Lead with:** the mechanism — a DAG / trace / diff, then a detail table.
- **Figure menu:** `VIZ.dag` (call chain, build graph), `VIZ.timeline`
  (span durations), `VIZ.table` with `code`/`status` cells (files, tools,
  errors), `VIZ.heat` (file × change, error × step).
- **Keep:** raw ids, exact durations, error text (as `code`, truncated),
  exit codes, token counts per step.
- **Cut:** business framing, ROI, "on track" language.

## 🙋 User lens

**Question they're answering:** "What happened, is it done, and what's next?"

- **Altitude:** medium. Steps as plain phases, not tool names.
- **Lead with:** a status header (one big badge) + a timeline of phases.
- **Figure menu:** `VIZ.timeline` (phases), `VIZ.kpis` (progress %, elapsed,
  result), `VIZ.table` (deliverables / outputs with status), `VIZ.donut`
  (done vs pending).
- **Keep:** outcomes, artifacts produced, blockers, ETA.
- **Cut:** tool ids, token internals, stack traces. Translate "ran `gh pr
  view`" → "checked the pull request".

## 🧭 Manager lens

**Question they're answering:** "Is the whole thing healthy, fast, and worth
it — and what's trending?"

- **Altitude:** high. Rollups across agents / PRs / time. Never single events.
- **Lead with:** a KPI tile row (health, throughput, cost, success rate),
  then distributions and trends.
- **Figure menu:** `VIZ.kpis` (with deltas), `VIZ.bars` (per-agent /
  per-author rollup), `VIZ.donut` (state mix), `VIZ.heat` (activity over
  time), `VIZ.table` (ranked rollup with `bar` cells).
- **Keep:** counts, rates, medians/p95, cost/token totals, week-over-week
  deltas, outliers flagged with status color.
- **Cut:** anything that requires reading a single trace. If a number can't
  be aggregated, it probably doesn't belong here.

---

## Choosing when unstated

Infer from wording: "debug / why / how / which file / trace" → developer;
"is it done / what did it do / status" → user; "how many / health / cost /
this week / across" → manager. Still unsure → **developer**, and drop a small
lens-switcher note pointing at the other two.

## Multi-lens pages

When asked to "cover all viewpoints", render **one page with three sections**
(Manager summary → User story → Developer detail), top-down by altitude, each
section a `<section>` with its own figures. Do **not** duplicate the same
chart three times — each lens gets figures tuned to its altitude.
