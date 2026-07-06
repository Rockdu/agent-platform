# Data contracts — where agent data lives and how it's shaped

Two data sources back the agent-related sub-skills. Both are documented here so
`visualize-agents` and `visualize-agent-trace` share one vocabulary. All shapes
verified against a live Agent Platform install (2026).

---

## 1. Claude session logs (on disk — always readable)

Path: `~/.claude/projects/<slug>/<sessionId>.jsonl`
The `<slug>` is the project cwd with `/` and `_`/spaces turned into `-`
(e.g. `/Users/x/agent-platform` → `-Users-x-agent-platform`). One file per
session; one JSON object per line ("envelope").

### Envelope (common fields)
```
type            "user" | "assistant" | "attachment" | "mode"
                | "permission-mode" | "ai-title" | "last-prompt"
                | "file-history-snapshot"
uuid            string            unique line id
parentUuid      string | null     previous line — forms the chain/tree
timestamp       ISO-8601 string
sessionId       string
cwd             string
gitBranch       string
isSidechain     bool              TRUE ⇒ a sub-agent (Task) turn
userType        "external" | ...
requestId       string            (assistant lines)
version         string            claude-code version
```

### Message payload (`type` = user | assistant)
`message.role` = "user" | "assistant"; `message.content` is an **array of
blocks**. Block types:
```
thinking      { type, thinking }                         (assistant)
text          { type, text }                             (assistant/user)
tool_use      { type, id, name, input, caller }          (assistant)
tool_result   { type, tool_use_id, content, is_error }   (user)
```
- `tool_use.name` — the tool (Bash, Read, Edit, Task, Skill, …).
- `tool_use.input` — tool args (e.g. `{command, description}` for Bash).
- `tool_use.caller` — `{type:"direct"}` or a skill/subagent origin.
- Pair a `tool_use` to its `tool_result` by `id === tool_use_id`.
- `is_error:true` on a result marks a failed step (render red).
- Assistant `message.usage` → `{ input_tokens, output_tokens,
  cache_read_input_tokens, ... }` for per-step cost.
- `message.model` → which model ran the turn.

### Reconstructing the processing chain
1. Read all lines, index by `uuid`.
2. Link `parentUuid → uuid` to get the ordered spine (and branches).
3. Walk in timestamp order; each assistant `tool_use` starts a **step**,
   closed by the matching `tool_result`. Duration = result.ts − use.ts.
4. Lines with `isSidechain:true` belong to a **sub-agent** spawned by a
   `Task` tool_use in the main line — group them as a nested lane.
`scripts/parse_session.py` (in `visualize-agent-trace`) does all of this and
emits normalized JSON.

---

## 2. Platform runtime state (live app memory)

The Tauri app holds this in Rust; it is exposed to the frontend via commands /
events, **not** as a stable on-disk file. Canonical shapes (from
`src/sidecar-status.ts` and `src/terminal-mesh.ts`):

### SidecarStatusSnapshot — one per plugin sidecar (MCP process)
```
clientId            string
pluginId            string       e.g. "papers", "example-notes"
pid                 number|null
state               string       "Ready" | "Spawning" | "BackingOff"
                                 | "Exited" | "Unrecoverable"
                                 | "ShuttingDown" | "TransportCorrupt"
generation          number       restart generation
recentRestartCount  number
nextBackoffMs       number
shutdownRequested   bool
```

### WorkspaceLifecycleSnapshot — one per terminal/agent tab
```
workspaceId            string|null
tabKind                "Workspace" | "Orchestrator" | ...
transportKind          "Local" | ...
status                 "Running" | "Done"
doneReason             string|null
lastActivityAtUnixMs   number
pendingLaunch          bool
agentBusy              bool
```

### How a skill obtains it
The skill process (a `claude` in a terminal) cannot call Tauri directly, so
use the first available of:
1. **Passed-in JSON** — the orchestrator collects snapshots and hands them to
   the skill as a file/stdin. Preferred and most reliable.
2. **A debug dump**, if the app writes one (check `~/AgentPlatform/` and the
   app support dir for a `*.json` state dump).
3. **Derive a proxy from disk** — enumerate `~/.claude/projects/*` for active
   sessions and `~/AgentPlatform/workspaces/*` for workspaces, and read
   `plugins/*/plugin.toml` for the set of installable sidecars. This yields
   structure + recent activity even when live PIDs/states are unavailable;
   mark such fields `unknown` (→ muted badge) rather than faking them.

`scripts/collect_agents.py` (in `visualize-agents`) implements #3 with hooks to
merge in #1/#2 when present, and **always records which fields are live vs
inferred** so the page can be honest about it.

---

## Honesty rule

Never fabricate a runtime value. If state is unknown, emit `"unknown"` (renders
as a muted badge) and note the source in the page footer's provenance line.
