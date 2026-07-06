---
name: visualize-agent-trace
description: >-
  Visualize a SINGLE agent's session as a SEGMENT RELATIONSHIP GRAPH — split the
  session into meaningful segments (one per human request), summarize what each
  part did, infer which parts are independent / parallelizable, and draw them as
  one DAG (dependencies as edges, parallel work in the same layer). Plus a
  timeline and a segment table. Self-contained, visual-first HTML. Use when the
  user wants to understand what one agent did and how its parts relate — NOT a
  flat list of tool calls. Reads a session .jsonl from ~/.claude/projects.
---

# visualize-agent-trace

One session → **a segment relationship graph**. Do NOT enumerate tool calls —
a human can't take that in. Summarize the session into a handful of meaningful
segments and show how they relate. North star: convey what the agent did, fast.

## 1. Get the raw material (don't visualize this directly)

```bash
python3 scripts/parse_session.py --list                      # pick a session
python3 scripts/parse_session.py --digest <session.jsonl>    # READ THIS
```
`--digest` splits the session at each **real human request** (the natural
segment boundary; tool-result turns and command injects are filtered) and
prints, per segment: the request, a coarse activity summary (runs / edits /
reads + files touched), the time span, and whether it hit errors. Typically
5–15 segments — the right altitude.

## 2. Summarize into a segment DAG (this is YOUR job)

Read the digest and produce a semantic segmentation — this is the step a script
can't do:

```js
nodes = [ { id, label,           // a SHORT semantic title of what this part did
            sub,                  // one incisive line (key output / files)
            group,               // the workstream/lane this belongs to
            status } ]            // ok / warn / err (from the segment's errors)
edges = [ { from, to, label } ]  // dependency: "then" / "enables" / "needed by"
```
Rules:
- **One node per meaningful segment.** Merge trivial back-to-back requests;
  keep the user's intent as the title's basis.
- **Edges = real dependencies.** If segment B needed A's output, `A → B`.
- **Parallelism falls out of layering:** segments with no dependency between
  them land in the **same layer** of `VIZ.dag` = visibly parallel. Give each
  independent workstream its own `group` so the lanes get distinct colors.
- Explicitly decide *which parts could have run in parallel* and reflect it by
  NOT drawing an edge between them.

## 3. Render — the DAG is the centerpiece

```js
VIZ.kpis("#kpis", [ {label:"Segments",value:n}, {label:"Parallel lanes",value:k}, … ]);
VIZ.dag("#dag", nodes, edges, { title:"段关系图 — 同层即可并行", colW:230 });
VIZ.timeline("#tl", tracks, { title:"actual timeline", fmt:t=>t+"m" });  // when each ran
VIZ.table("#tbl", cols, rows, { title:"segments", sortable:false });     // what each did
```
Lead with the DAG. Add the timeline (actual timing — often reveals that a
parallelizable lane was actually done serially) and a compact segment table.
Nothing else unless asked. See `examples/agent-trace-segments.html`.

## 4. Assemble & self-check

Build from `../visualize-anything/references/html-shell.md`, inline
`viz.css`/`viz.js`, embed the segment `DATA`, run the visual-first self-check,
write `agent-trace-<shortid>.html`, report the path.
