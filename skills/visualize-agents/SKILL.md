---
name: visualize-agents
description: >-
  Visualize the agent platform ITSELF — how its agents relate and what each is
  doing right now — as a self-contained, visual-first HTML page built around an
  interactive AGENT MAP: rounded-rect cards (one per agent) showing an incisive
  one-line "what it's doing now", connected by LABELED relationship edges
  (manages / spawns / hosts / uses), each card expandable in place to a
  condensed trace. Use when the user wants the big picture of the platform's
  agents, their relationships, and live status. Reads ~/.claude/projects/** and,
  when provided, live platform runtime state. Pairs with visualize-anything.
---

# visualize-agents

North star (overrides everything): **convey what the agents are and how they
relate, as fast as possible.** The agent map IS the page. Do not bury it under
KPI rows, donuts, and tables that don't speed understanding of the
relationships — cut anything that doesn't serve instant comprehension.

## 1. Get the data

```bash
python3 scripts/collect_agents.py                        # inferred from logs
python3 scripts/collect_agents.py --runtime runtime.json # merge live state
```
The output's `agentmap` field is ready for `VIZ.agentmap`. Nodes carry:
`doing` = the **latest human request** (what the agent is working on now, in the
user's own words — never a raw tool call); `topic` = session ai-title;
`status`; and a condensed `trace` whose `steps` are the session's **segments**
(one per human request) with a coarse activity line each — NOT a tool-call list.
Edges carry a relationship `label`. Embed as the inline `DATA` literal.

## 2. Build the page — agent map front and center

```js
VIZ.agentmap("#map", DATA.agentmap, { title: "Agents · relationships & status" });
```
- **Nodes** are rounded-rect cards. The face shows the agent name + status
  badge + **one incisive line of what it's doing now**; a muted subline shows
  its topic. Layered top→down: orchestrator → agents → (sub-agents), plus
  hosted sidecars.
- **Edges are labeled** with the relationship type (`manages`, `terminal
  session`, `spawns`, `hosts`, `uses`) and follow the cards when they reflow.
- **Expand in place**: clicking a card opens its condensed **来龙去脉** — a
  3–4 beat causal narrative you AUTHOR per agent (never a request or tool-call
  list). Set it as `node.trace.story`:

  ```js
  story: [
    {text:"仓库开不了 · 无一键安装", status:"err", because:"先让它能跑"},
    {text:"手动跑通并固化成幂等脚本", status:"ok", because:"环境缺依赖"},
    {text:"补依赖发现 + 集成 login",  status:"ok"},
  ]
  ```
  Each beat = one incisive line of what happened (status colors the dot);
  `because` = why it led to the next beat. Author it by reading
  `parse_session.py --digest` (in visualize-agent-trace) for each session and
  summarizing the arc — the collector's per-request segments are raw material,
  not the display. Drop `trace.steps` from the display. For one agent's full
  narrative, use `visualize-agent-trace`.

That is the whole page for the default view. Add at most a **one-line context
strip** (agents · active · errors) above the map if it genuinely helps — no
more. Only if the user explicitly asks for a developer status board or manager
rollup should you add the sessions/sidecars tables or KPI/donut figures; even
then, the map stays on top.

## 3. Runtime honesty

Without `--runtime`, agent status is inferred from session recency and sidecar
states are `unknown` (muted badge, "state unknown" on the card). Put one muted
footer line saying so. Never render an `unknown` as a definite state.

## 4. Assemble & self-check

Build from `../visualize-anything/references/html-shell.md`, inline
`viz.css`/`viz.js`, embed `DATA`, run the visual-first self-check, write
`agents-overview.html`, report the path.
