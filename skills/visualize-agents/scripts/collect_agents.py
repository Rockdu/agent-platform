#!/usr/bin/env python3
"""Collect the agent-platform's agents, their relationships, and status.

Builds a normalized JSON model of every agent (Claude session) across all
projects, the plugin sidecars, and the relationship graph between them —
for the visualize-agents skill to embed into a self-contained HTML page.

Data sources, in order of trust (see references/data-contracts.md):
  1. --runtime FILE : JSON of live SidecarStatusSnapshot[] /
                      WorkspaceLifecycleSnapshot[] handed in by the
                      orchestrator. Merged when present ("live").
  2. plugin.toml    : the installable sidecar set, read from --platform-dir.
  3. session logs   : ~/.claude/projects/**  (always available). Recency is
                      used as an *inferred* activity proxy.

Every field records whether it is live or inferred; unknowns stay "unknown"
(never fabricated).

Usage:
    collect_agents.py [--platform-dir ~/agent-platform]
                      [--runtime runtime.json]
                      [--active-min 5] [--idle-min 60]
"""
import json
import os
import re
import sys
import glob
import argparse
from datetime import datetime

PROJECTS = os.path.expanduser("~/.claude/projects")
NOW = None  # set from newest mtime to stay deterministic vs. wall clock


def _short(s, n):
    s = " ".join(str(s or "").split())
    return s if len(s) <= n else s[: n - 1] + "…"


_CAT = {"Read": "read", "Grep": "read", "Glob": "read",
        "Edit": "edit", "Write": "edit", "NotebookEdit": "edit",
        "Bash": "run", "Task": "delegate", "Skill": "skill"}


def _cat(name):
    return "mcp" if str(name).startswith("mcp__") else _CAT.get(name, "other")


def is_real_prompt(o):
    """A genuine human turn (segment boundary): a user message with non-empty
    text that is not a command/tool-notification inject (those start with '<')."""
    if o.get("type") != "user":
        return False
    txt = prompt_text(o)
    return bool(txt) and not txt.startswith("<")


def prompt_text(o):
    c = o.get("message", {}).get("content") if isinstance(o.get("message"), dict) else None
    if isinstance(c, str):
        return c.strip()
    if isinstance(c, list):
        for b in c:
            if isinstance(b, dict) and b.get("type") == "text":
                return (b.get("text") or "").strip()
    return ""


def load_jsonl_meta(path):
    """Scan one session file and segment it by the user's real requests.

    Each human turn opens a segment; we record what the agent DID in that
    segment as coarse counts (runs / edits / reads) + files — never per-call
    detail. `doing` is the latest request (the current task, in the user's own
    words). `segments` feeds the card + is the raw material for a segment DAG."""
    m = {"turns": 0, "tools": 0, "errors": 0, "model": None,
         "subagents": 0, "sessionId": None, "cwd": None,
         "tools_by_name": {}, "firstTs": None, "lastTs": None,
         "title": None, "lastPrompt": None, "entrypoint": None,
         "tokensOut": 0, "doing": None, "segments": []}
    segs = []
    cur = None
    try:
        fh = open(path)
    except OSError:
        return m
    with fh:
        for ln in fh:
            ln = ln.strip()
            if not ln:
                continue
            try:
                o = json.loads(ln)
            except json.JSONDecodeError:
                continue
            m["sessionId"] = m["sessionId"] or o.get("sessionId")
            m["cwd"] = m["cwd"] or o.get("cwd")
            m["entrypoint"] = m["entrypoint"] or o.get("entrypoint")
            ts = o.get("timestamp")
            if ts:
                m["firstTs"] = m["firstTs"] or ts
                m["lastTs"] = ts
            t = o.get("type")
            if t == "ai-title":
                m["title"] = o.get("aiTitle") or o.get("title") or m["title"]
            elif t == "last-prompt":
                m["lastPrompt"] = o.get("lastPrompt") or o.get("prompt") or m["lastPrompt"]
            if t in ("user", "assistant"):
                m["turns"] += 1
            if is_real_prompt(o):
                cur = {"title": _short(prompt_text(o), 64), "run": 0, "edit": 0,
                       "read": 0, "other": 0, "files": [], "errors": 0, "ts": ts}
                segs.append(cur)
            msg = o.get("message")
            if not isinstance(msg, dict):
                continue
            if msg.get("model"):
                m["model"] = msg.get("model")
            m["tokensOut"] += int((msg.get("usage") or {}).get("output_tokens", 0) or 0)
            for b in msg.get("content", []) if isinstance(msg.get("content"), list) else []:
                if not isinstance(b, dict):
                    continue
                if b.get("type") == "tool_use":
                    m["tools"] += 1
                    nm = b.get("name", "?")
                    m["tools_by_name"][nm] = m["tools_by_name"].get(nm, 0) + 1
                    cat = _cat(nm)
                    if nm == "Task":
                        m["subagents"] += 1
                    if cur is None:
                        cur = {"title": "(startup)", "run": 0, "edit": 0, "read": 0,
                               "other": 0, "files": [], "errors": 0, "ts": ts}
                        segs.append(cur)
                    bucket = cat if cat in ("run", "edit", "read") else "other"
                    cur[bucket] += 1
                    inp = b.get("input")
                    if cat in ("edit", "read") and isinstance(inp, dict) and inp.get("file_path"):
                        fn = os.path.basename(str(inp["file_path"]))
                        if fn and fn not in cur["files"]:
                            cur["files"].append(fn)
                elif b.get("type") == "tool_result" and b.get("is_error"):
                    m["errors"] += 1
                    if cur:
                        cur["errors"] += 1
    m["segments"] = segs
    reqs = [s["title"] for s in segs if s["title"] != "(startup)"]
    m["doing"] = (reqs[-1] if reqs else (m["title"] or (_short(m["lastPrompt"], 64) if m["lastPrompt"] else "idle")))
    return m


def slug_to_label(slug):
    # "-Users-x-AgentPlatform-workspaces-foo" -> "foo"
    parts = slug.strip("-").split("-")
    if "workspaces" in parts:
        return "-".join(parts[parts.index("workspaces") + 1:]) or slug
    return parts[-1] if parts else slug


def read_plugins(platform_dir):
    out = []
    pdir = os.path.join(platform_dir, "plugins")
    for toml in glob.glob(os.path.join(pdir, "*", "plugin.toml")):
        pid = os.path.basename(os.path.dirname(toml))
        name = pid
        try:
            txt = open(toml).read()
            mm = re.search(r'(?m)^\s*(?:id|name)\s*=\s*"([^"]+)"', txt)
            if mm:
                name = mm.group(1)
        except OSError:
            pass
        out.append({"pluginId": pid, "name": name})
    return out


def status_from_age(age_min, active_min, idle_min):
    if age_min is None:
        return "unknown"
    if age_min <= active_min:
        return "busy"    # active
    if age_min <= idle_min:
        return "idle"
    return "done"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--platform-dir", default=os.path.expanduser("~/agent-platform"))
    ap.add_argument("--runtime", default=None)
    ap.add_argument("--active-min", type=float, default=5)
    ap.add_argument("--idle-min", type=float, default=60)
    a = ap.parse_args()

    files = glob.glob(os.path.join(PROJECTS, "*", "*.jsonl"))
    global NOW
    mtimes = [os.stat(f).st_mtime for f in files if os.path.exists(f)]
    NOW = max(mtimes) if mtimes else datetime.now().timestamp()

    projects = {}   # slug -> project dict
    sessions = []
    for f in files:
        slug = os.path.basename(os.path.dirname(f))
        st = os.stat(f)
        age_min = (NOW - st.st_mtime) / 60.0
        meta = load_jsonl_meta(f)
        status = status_from_age(age_min, a.active_min, a.idle_min)
        # key on the file stem: it is unique per session file, whereas the
        # internal sessionId can repeat across resumed sessions.
        sid = os.path.splitext(os.path.basename(f))[0]
        sess = {
            "id": sid, "short": sid[:8], "project": slug,
            "reportedSessionId": meta["sessionId"],
            "projectLabel": slug_to_label(slug),
            "sizeBytes": st.st_size, "ageMin": round(age_min, 1),
            "turns": meta["turns"], "tools": meta["tools"],
            "errors": meta["errors"], "subagents": meta["subagents"],
            "model": meta["model"], "status": status,
            "statusSource": "inferred(recency)",
            "topTools": sorted(meta["tools_by_name"].items(), key=lambda x: -x[1])[:4],
            "title": meta["title"], "doing": meta["doing"],
            "entrypoint": meta["entrypoint"], "tokensOut": meta["tokensOut"],
            "segments": meta["segments"],
        }
        sessions.append(sess)
        p = projects.setdefault(slug, {
            "slug": slug, "label": slug_to_label(slug), "cwd": meta["cwd"],
            "sessions": 0, "turns": 0, "tools": 0, "errors": 0,
            "subagents": 0, "minAge": age_min,
        })
        p["sessions"] += 1
        p["turns"] += meta["turns"]
        p["tools"] += meta["tools"]
        p["errors"] += meta["errors"]
        p["subagents"] += meta["subagents"]
        p["minAge"] = min(p["minAge"], age_min)
    for p in projects.values():
        p["status"] = status_from_age(p["minAge"], a.active_min, a.idle_min)
        p["minAge"] = round(p["minAge"], 1)

    # sidecars: installable set from plugin.toml, live state merged if provided
    plugins = read_plugins(a.platform_dir)
    sidecars = {p["pluginId"]: {
        "pluginId": p["pluginId"], "name": p["name"],
        "state": "unknown", "pid": None, "recentRestartCount": None,
        "source": "plugin.toml(installed set)",
    } for p in plugins}

    runtime_present = False
    lifecycle = []
    if a.runtime and os.path.isfile(a.runtime):
        runtime_present = True
        try:
            rt = json.load(open(a.runtime))
        except Exception:
            rt = {}
        for s in rt.get("sidecars", []) if isinstance(rt, dict) else []:
            pid = s.get("pluginId")
            sc = sidecars.setdefault(pid, {"pluginId": pid, "name": pid})
            sc.update({"state": s.get("state", "unknown"), "pid": s.get("pid"),
                       "recentRestartCount": s.get("recentRestartCount"),
                       "source": "runtime(live)"})
        lifecycle = rt.get("lifecycle", []) if isinstance(rt, dict) else []

    # relationship graph: platform hub -> projects -> (sidecars used)
    nodes = [{"id": "platform", "label": "agent-platform", "group": "platform",
              "status": "busy" if any(p["status"] == "busy" for p in projects.values()) else "idle",
              "count": len(sessions)}]
    edges = []
    for slug, p in projects.items():
        nid = "proj:" + slug
        nodes.append({"id": nid, "label": p["label"], "group": "project",
                      "status": p["status"], "count": p["sessions"]})
        edges.append({"from": "platform", "to": nid})
    for pid, sc in sidecars.items():
        nid = "plugin:" + pid
        nodes.append({"id": nid, "label": sc["name"], "group": "sidecar",
                      "status": sc["state"]})
        edges.append({"from": "platform", "to": nid})

    active = sum(1 for s in sessions if s["status"] == "busy")
    total_errors = sum(s["errors"] for s in sessions)

    # ---- agent map: labeled relationships + per-agent "doing" + trace -------
    # The primary view. Hub = the single orchestrator (platform architecture);
    # it MANAGES each workspace agent, HOSTS each sidecar; an agent that used
    # the Task tool SPAWNS a sub-agent group. Relationships are labeled.
    def seg_activity(seg):
        parts = []
        if seg["run"]:
            parts.append(f'{seg["run"]} run' + ("s" if seg["run"] != 1 else ""))
        if seg["edit"]:
            parts.append(f'{seg["edit"]} edit' + ("s" if seg["edit"] != 1 else ""))
        if seg["read"]:
            parts.append(f'{seg["read"]} read' + ("s" if seg["read"] != 1 else ""))
        if seg["other"]:
            parts.append(f'{seg["other"]} other')
        line = " · ".join(parts)
        if seg["files"]:
            line += "  (" + ", ".join(seg["files"][:3]) + ("…" if len(seg["files"]) > 3 else "") + ")"
        return line

    am_nodes = [{
        "id": "orchestrator", "label": "orchestrator", "kind": "orchestrator",
        "status": "busy" if active else "idle",
        "doing": f"coordinating {len(sessions)} agents across {len(projects)} workspaces",
        "topic": None,
    }]
    am_edges = []
    for s in sessions:
        nid = "agent:" + s["id"]
        by_entry = {"claude-desktop": "launched by app", "cli": "terminal session"}
        am_nodes.append({
            "id": nid, "kind": "agent", "status": s["status"],
            "label": s["projectLabel"] + " · " + s["short"],
            "doing": s["doing"] or "idle",
            "topic": s["title"],
            "trace": {
                "kpis": [
                    {"label": "turns", "value": s["turns"]},
                    {"label": "tools", "value": s["tools"]},
                    {"label": "errors", "value": s["errors"]},
                    {"label": "tok out", "value": f'{s["tokensOut"]:,}'},
                ],
                "summary": " · ".join(filter(None, [
                    f'{sum(x[1] for x in s["topTools"])}+ tool calls in {len(s["segments"])} segments'])),
                "steps": [{"label": seg["title"], "sub": seg_activity(seg),
                           "status": "err" if seg["errors"] else "ok"}
                          for seg in s["segments"][-6:]],
            },
        })
        am_edges.append({"from": "orchestrator", "to": nid,
                         "label": by_entry.get(s["entrypoint"], "manages")})
        if s["subagents"]:
            snid = "sub:" + s["id"]
            am_nodes.append({"id": snid, "kind": "sub-agents", "status": s["status"],
                             "label": f'{s["subagents"]} sub-agent(s)',
                             "doing": "spawned via Task tool", "topic": None})
            am_edges.append({"from": nid, "to": snid, "label": "spawns"})
    for pid, sc in sidecars.items():
        snid = "plugin:" + pid
        am_nodes.append({"id": snid, "kind": "sidecar", "status": sc["state"],
                         "label": sc["name"],
                         "doing": ("MCP plugin · " + sc["state"]) if sc["state"] != "unknown"
                                  else "MCP plugin · state unknown",
                         "topic": None})
        am_edges.append({"from": "orchestrator", "to": snid, "label": "hosts"})

    out = {
        "kpis": [
            {"label": "Projects", "value": len(projects), "status": "info"},
            {"label": "Agent sessions", "value": len(sessions), "status": "info"},
            {"label": "Active now", "value": active, "status": "busy" if active else "idle",
             "note": f"≤{a.active_min:g}m"},
            {"label": "Sidecars", "value": len(sidecars),
             "status": "ok" if runtime_present else "muted",
             "note": "live" if runtime_present else "inferred"},
            {"label": "Errors (all)", "value": total_errors,
             "status": "err" if total_errors else "ok"},
        ],
        "projects": sorted(projects.values(), key=lambda p: p["minAge"]),
        "sessions": sorted(sessions, key=lambda s: s["ageMin"]),
        "sidecars": list(sidecars.values()),
        "lifecycle": lifecycle,
        "graph": {"nodes": nodes, "edges": edges},
        "agentmap": {"nodes": am_nodes, "edges": am_edges},
        "meta": {
            "generatedFrom": PROJECTS,
            "platformDir": a.platform_dir,
            "runtime": "live" if runtime_present else "not provided — states inferred from recency",
            "referenceNow": datetime.fromtimestamp(NOW).isoformat(),
        },
    }
    json.dump(out, sys.stdout, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
