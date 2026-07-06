# HTML shell + VIZ engine usage

Every page is one self-contained `.html`: inline `viz.css` in `<head>`, inline
`viz.js` before `</body>`, embed data as a JSON literal, render in a
`DOMContentLoaded` handler. No external requests — safe as a file *and* as a
Claude Artifact.

## Page skeleton (copy, then fill)

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{TITLE}}</title>
<style>/* ← paste the entire contents of assets/viz.css here */</style>
</head>
<body>
<div class="viz-page">
  <header class="viz-head">
    <h1>{{TITLE}}</h1>
    <span class="viz-scope">{{SCOPE}}</span>
  </header>
  <div class="viz-meta">
    <span>source: <code>{{SOURCE}}</code></span>
    <span>generated: {{WHEN}}</span>
    <span class="viz-lens">lens:
      <span class="viz-chip" style="--b:var(--cat-0)">{{LENS}}</span>
    </span>
  </div>

  <!-- Manager altitude: KPI row first -->
  <div id="kpis"></div>

  <!-- Small multiples: 2–3 up -->
  <div class="viz-grid cols-2" style="margin-top:16px">
    <div id="fig-a"></div>
    <div id="fig-b"></div>
  </div>

  <!-- Full-width structural figure -->
  <div style="margin-top:16px"><div id="fig-dag"></div></div>

  <!-- One dense detail table -->
  <div style="margin-top:16px"><div id="fig-table"></div></div>

  <footer class="viz-footer">
    <span>{{PROVENANCE}}</span><span>visualize-anything</span>
  </footer>
</div>

<script>/* ← paste the entire contents of assets/viz.js here */</script>
<script>
const DATA = /* ← inline your normalized JSON here */ {};
document.addEventListener("DOMContentLoaded", () => {
  VIZ.kpis("#kpis", DATA.kpis);
  // … render each figure from DATA …
});
</script>
</body>
</html>
```

To publish as an Artifact instead of a local file, write the same content and
call the Artifact tool — the strict CSP is already satisfied because nothing
is external.

## VIZ API — data shapes

```js
// KPI tiles
VIZ.kpis("#kpis", [
  { label:"Success rate", value:"92", unit:"%", delta:"+4", status:"ok", note:"7d" },
  { label:"Active agents", value:6, status:"busy" },
]);

// Sortable table. col.type: text|num|status|bar|code|chips
VIZ.table("#t", [
  { key:"name", label:"Agent" },
  { key:"state", label:"State", type:"status" },
  { key:"calls", label:"Tool calls", type:"num" },
  { key:"share", label:"Share", type:"bar" },
  { key:"tools", label:"Tools", type:"chips" },
], rows, { title:"Agents", sortable:true, sortCol:2, sortDir:"desc" });

// Bar chart (horizontal default; orient:"vertical" for columns)
VIZ.bars("#b", [{ label:"Bash", value:216, status:"busy" }], { title:"Tool usage" });

// Donut with center label
VIZ.donut("#d", [{ label:"ready", value:5, status:"ok" }, { label:"exited", value:1, status:"err" }],
  { title:"Sidecar states", center:"6", centerSub:"sidecars" });

// Layered DAG (acyclic) — call chain / build graph
VIZ.dag("#dag",
  [{ id:"a", label:"Read", sub:"file.ts", status:"ok" }, { id:"b", label:"Edit", status:"ok" }],
  [{ from:"a", to:"b" }], { title:"Processing chain" });

// Radial relationship graph (cycles ok) — set hub for a center node
VIZ.graph("#g", nodes, edges, { title:"Agent relationships", hub:"orchestrator" });

// Timeline / gantt. times are numbers (epoch ms or relative); fmt formats ticks
VIZ.timeline("#tl", [
  { label:"orchestrator", bars:[{ start:0, end:12000, label:"plan", status:"busy" }] },
], { title:"Session timeline", fmt:(t)=> (t/1000).toFixed(0)+"s" });

// Heatmap
VIZ.heat("#h", { rows:["mon","tue"], cols:["a","b"], values:[[1,4],[2,0]] },
  { title:"Activity", showValues:true });

// Agent map — labeled relationships + expandable "doing" cards.
// node.doing = ONE incisive line (what it's doing now); node.topic = muted subline;
// edge.label = relationship type; node.trace = condensed detail shown on expand.
VIZ.agentmap("#map", {
  nodes:[
    { id:"orch", label:"orchestrator", kind:"orchestrator", status:"busy", doing:"coordinating 3 agents" },
    { id:"a1", label:"repoA · 1a2b", kind:"agent", status:"busy",
      doing:"Editing collect_agents.py", topic:"build the installer",
      trace:{ kpis:[{label:"turns",value:40},{label:"errors",value:1}],
              tools:[{label:"Bash",value:70,status:"busy"}],
              steps:[{label:"Bash",sub:"cargo build",status:"ok"},{label:"Edit",sub:"main.rs",status:"err"}] } },
    { id:"s1", label:"papers", kind:"sidecar", status:"unknown", doing:"MCP plugin · state unknown" },
    // preferred card detail: trace.story = condensed causal narrative (来龙去脉)
    // [{text, status, because?}] — `because` explains why the next beat happened
  ],
  edges:[ {from:"orch",to:"a1",label:"manages"}, {from:"orch",to:"s1",label:"hosts"} ],
}, { title:"Agents", open:false });   // open:true expands every card by default
```

## Rules of thumb

- **status** strings are mapped semantically (see `viz.js` `statusVar`):
  ready/ok/merged→green, warn/pending/review→amber, err/exited/failed→red,
  busy/running/spawning→blue, idle/queued→grey. Pass raw platform states
  (`Ready`, `BackingOff`, `Exited`…) directly; they're normalized.
- **catColor(i)** is the categorical ring — use for series, groups, authors.
- Keep node/label text short; put full text in `title=` (native tooltip).
- For big traces (100+ steps) prefer **timeline + table** over a giant DAG;
  reserve `VIZ.dag` for the structural/subagent skeleton.
- One page = one palette. Don't recolor per figure.
