# visualize-anything — a visual-first skill set

Turn almost anything into **one self-contained HTML page** that leads with
charts, relationship graphs, DAGs, timelines, and tables — with prose kept to
a minimum. Built for the Agent Platform, but the engine is generic.

> **Hard rule:** every generated page has more visual elements than text
> blocks. Figures and tables carry the meaning; prose is the exception.

## Structure — a router + sub-skills

```
skills/
  visualize-anything/       ← ROUTER + shared engine (start here)
    SKILL.md                  picks (viewpoint × category), dispatches
    assets/viz.css            palette + layout (theme-aware, print, no CDN)
    assets/viz.js             renderers: kpis, table, bars, donut, dag,
                              graph, timeline, heat, tree, sparkline
    references/               viewpoints · visual-first · chart-chooser
                              · html-shell · data-contracts
  visualize-repo/           ← a repository (git/gh): activity, hotspots, mix
  visualize-pr/             ← a pull request (gh): diff, reviews, checks, stack
  visualize-agents/         ← the platform's agents: relationships + runtime
    scripts/collect_agents.py
  visualize-agent-trace/    ← one agent's processing chain (session .jsonl)
    scripts/parse_session.py
```

## Two axes

**Viewpoint** — 👩‍💻 developer (mechanism, failures) · 🙋 user (outcome,
progress) · 🧭 manager (health, throughput, cost). The lens changes *which
figures lead and how much you aggregate*, not just the labels.

**Category** — repository · pull request · the agents & their runtime status ·
a single agent's trace. Anything else is built from the same primitives.

## Data sources

- **Repo / PR:** `git` + `gh` CLI.
- **The agents:** `~/.claude/projects/**` session logs (always readable) +
  live platform runtime state when the orchestrator passes it in
  (`--runtime`). States are never faked — unknowns render as muted badges and
  are noted in the footer.
- **One trace:** a single session `.jsonl`, parsed into normalized steps.

## Output

A single `.html` file: `viz.css` inlined in `<head>`, `viz.js` inlined before
`</body>`, data embedded as a JSON literal, rendered on `DOMContentLoaded`.
**Zero external requests** — open it as a file, or publish it as a Claude
Artifact (its CSP is already satisfied).

## Install

```bash
./install.sh            # symlink skills into ~/.claude/skills
./install.sh --copy     # hard copy instead
./install.sh --uninstall
```
Then in Claude Code: *"visualize this repo / this PR / the agents / this
agent's trace"* (add "for a manager/user/developer" to pick the lens; "cover
all viewpoints" for a three-section page).

## Design credits

The figure choices and rules distill common practice from LLM-trace UIs
(LangSmith, Langfuse, Phoenix, Weave, AgentOps), repo viz (Gource,
git-of-theseus, GitHub Insights, CodeScene), and the dataviz canon (Tufte,
Few, NN/g, FT Visual Vocabulary). See `references/chart-chooser.md`.
