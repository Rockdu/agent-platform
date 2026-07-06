#!/usr/bin/env python3
"""Parse one Claude Code session .jsonl into a normalized trace JSON.

Reconstructs the processing chain: ordered steps (tool_use paired with its
tool_result), thinking/text turns, sub-agent (sidechain) lanes, per-step
duration and token usage, and errors. Emits JSON on stdout for the
visualize-agent-trace skill to embed into a self-contained HTML page.

Usage:
    parse_session.py <session.jsonl>
    parse_session.py --latest [<project-slug-or-cwd>]   # newest session
    parse_session.py --list                             # list sessions

Output shape (stdout):
{
  "session": {id, cwd, gitBranch, model, version, startedAt, endedAt,
              durationMs, numSteps, numErrors, tokensIn, tokensOut},
  "steps":  [{i, kind, tool, title, detail, status, startMs, endMs,
              durMs, tokensOut, sidechain, uuid, parentUuid}],
  "tools":  [{name, count, errors, totalMs}],           # rollup
  "phases": [{label, startMs, endMs, status}],          # user-lens phases
  "meta":   {source, generatedNote}
}
Times are milliseconds relative to session start.
"""
import json
import os
import sys
import glob
from datetime import datetime

PROJECTS = os.path.expanduser("~/.claude/projects")


def parse_ts(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() * 1000.0
    except Exception:
        return None


def list_sessions():
    out = []
    for f in glob.glob(os.path.join(PROJECTS, "*", "*.jsonl")):
        try:
            st = os.stat(f)
        except OSError:
            continue
        out.append((st.st_mtime, st.st_size, f))
    out.sort(reverse=True)
    return out


def resolve(arg):
    """Resolve --latest [slug|cwd] or a direct path to a jsonl file."""
    if arg and os.path.isfile(arg):
        return arg
    sessions = list_sessions()
    if not sessions:
        sys.exit("no sessions under " + PROJECTS)
    if arg:
        key = arg.replace("/", "-").replace("_", "-").strip("-")
        for _, _, f in sessions:
            if key in f:
                return f
    return sessions[0][2]


def short(s, n):
    s = " ".join(str(s or "").split())
    return s if len(s) <= n else s[: n - 1] + "…"


_CAT = {"Read": "read", "Grep": "read", "Glob": "read",
        "Edit": "edit", "Write": "edit", "NotebookEdit": "edit",
        "Bash": "run", "Task": "delegate", "Skill": "skill"}


def cat_of(name):
    return "mcp" if str(name).startswith("mcp__") else _CAT.get(name, "other")


def prompt_text(o):
    c = o.get("message", {}).get("content") if isinstance(o.get("message"), dict) else None
    if isinstance(c, str):
        return c.strip()
    if isinstance(c, list):
        for b in c:
            if isinstance(b, dict) and b.get("type") == "text":
                return (b.get("text") or "").strip()
    return ""


def is_real_prompt(o):
    """A genuine human turn = user message with non-empty text that is not a
    command/tool-notification inject ('<...>') or an interrupt marker."""
    if o.get("type") != "user":
        return False
    txt = prompt_text(o)
    return bool(txt) and not txt.startswith("<") and not txt.startswith("[Request interrupted")


def segment(lines, t0):
    """Split the session into segments at each real human request. Each segment
    records what the agent DID as coarse counts + files — never per-call detail.
    This is the raw material a human (or Claude) turns into a labeled segment
    DAG (what each part did, which parts are parallel)."""
    segs, cur = [], None
    for o in lines:
        rel = parse_ts(o.get("timestamp"))
        rel = int(rel - t0) if rel is not None else None
        if is_real_prompt(o):
            cur = {"i": len(segs), "request": short(prompt_text(o), 90),
                   "run": 0, "edit": 0, "read": 0, "delegate": 0, "other": 0,
                   "files": [], "errors": 0, "startMs": rel, "endMs": rel}
            segs.append(cur)
        msg = o.get("message")
        if not isinstance(msg, dict) or not isinstance(msg.get("content"), list):
            continue
        for b in msg["content"]:
            if not isinstance(b, dict):
                continue
            if b.get("type") == "tool_use":
                if cur is None:
                    cur = {"i": 0, "request": "(startup)", "run": 0, "edit": 0,
                           "read": 0, "delegate": 0, "other": 0, "files": [],
                           "errors": 0, "startMs": rel, "endMs": rel}
                    segs.append(cur)
                cat = cat_of(b.get("name", "?"))
                cur[cat if cat in ("run", "edit", "read", "delegate") else "other"] += 1
                if rel is not None:
                    cur["endMs"] = rel
                inp = b.get("input")
                if cat in ("edit", "read") and isinstance(inp, dict) and inp.get("file_path"):
                    fn = os.path.basename(str(inp["file_path"]))
                    if fn and fn not in cur["files"]:
                        cur["files"].append(fn)
            elif b.get("type") == "tool_result" and b.get("is_error") and cur:
                cur["errors"] += 1
    return segs


def seg_activity(s):
    parts = []
    for k, lab in (("run", "run"), ("edit", "edit"), ("read", "read"), ("delegate", "delegate"), ("other", "other")):
        if s[k]:
            parts.append(f'{s[k]} {lab}' + ("s" if s[k] != 1 and lab != "other" else ""))
    line = " · ".join(parts) or "no tools"
    if s["files"]:
        line += "  (" + ", ".join(s["files"][:4]) + ("…" if len(s["files"]) > 4 else "") + ")"
    return line


def tool_title(name, inp):
    """A compact human title for a tool_use."""
    if not isinstance(inp, dict):
        return name, ""
    if name == "Bash":
        return name, short(inp.get("description") or inp.get("command"), 60)
    if name in ("Read", "Edit", "Write", "NotebookEdit"):
        return name, short(os.path.basename(str(inp.get("file_path", ""))), 40)
    if name in ("Grep", "Glob"):
        return name, short(inp.get("pattern") or inp.get("query"), 40)
    if name == "Task":
        return "Task", short(inp.get("description") or inp.get("subagent_type"), 40)
    if name == "Skill":
        return "Skill", short(inp.get("skill") or inp.get("command"), 40)
    for k in ("description", "query", "prompt", "path", "url", "command"):
        if inp.get(k):
            return name, short(inp[k], 50)
    return name, ""


def main():
    args = sys.argv[1:]
    if args and args[0] == "--list":
        for mt, sz, f in list_sessions():
            print(f"{datetime.fromtimestamp(mt):%Y-%m-%d %H:%M}  {sz:>9}  {f}")
        return
    digest = False
    if args and args[0] == "--digest":
        digest = True
        args = args[1:]
    if args and args[0] == "--latest":
        args = args[1:]
    path = resolve(args[0] if args else None)

    lines = []
    with open(path) as fh:
        for ln in fh:
            ln = ln.strip()
            if not ln:
                continue
            try:
                lines.append(json.loads(ln))
            except json.JSONDecodeError:
                continue

    ts = [parse_ts(o.get("timestamp")) for o in lines]
    t0 = next((t for t in ts if t is not None), 0.0)
    tN = next((t for t in reversed(ts) if t is not None), t0)

    segments = segment(lines, t0)

    # --digest: a compact, human/Claude-readable segmentation to summarize from.
    # Read this, then author a semantic segment DAG (title + summary + dependsOn
    # + parallel lanes) for VIZ.dag — see the visualize-agent-trace SKILL.
    if digest:
        title = next((o.get("aiTitle") for o in lines if o.get("type") == "ai-title" and o.get("aiTitle")), None)
        print(f"SESSION  {os.path.basename(path)}")
        if title:
            print(f"TITLE    {title}")
        print(f"SPAN     {int((tN - t0) / 60000)} min · {len(segments)} segments (by human request)\n")
        for s in segments:
            mm = f'[{int((s["startMs"] or 0) / 60000)}–{int((s["endMs"] or 0) / 60000)}m]'
            flag = "  ⚠ errors" if s["errors"] else ""
            print(f'#{s["i"]:<2} {mm:<12} {s["request"]}')
            print(f'        did: {seg_activity(s)}{flag}\n')
        return

    # index tool_result by tool_use_id for pairing
    result_by_id = {}
    for o in lines:
        msg = o.get("message")
        if not isinstance(msg, dict):
            continue
        for b in msg.get("content", []) if isinstance(msg.get("content"), list) else []:
            if isinstance(b, dict) and b.get("type") == "tool_result":
                result_by_id[b.get("tool_use_id")] = (o, b)

    sess = {
        "id": None, "cwd": None, "gitBranch": None, "model": None,
        "version": None, "startedAt": None, "endedAt": None,
        "durationMs": int(tN - t0), "numSteps": 0, "numErrors": 0,
        "tokensIn": 0, "tokensOut": 0,
    }
    steps = []
    tools = {}

    for o in lines:
        typ = o.get("type")
        msg = o.get("message")
        rel = parse_ts(o.get("timestamp"))
        rel = int(rel - t0) if rel is not None else None
        sess["id"] = sess["id"] or o.get("sessionId")
        sess["cwd"] = sess["cwd"] or o.get("cwd")
        if o.get("gitBranch"):
            sess["gitBranch"] = o.get("gitBranch")
        sess["version"] = sess["version"] or o.get("version")
        if sess["startedAt"] is None and o.get("timestamp"):
            sess["startedAt"] = o.get("timestamp")
        if o.get("timestamp"):
            sess["endedAt"] = o.get("timestamp")
        if not isinstance(msg, dict):
            continue
        if msg.get("model"):
            sess["model"] = msg.get("model")
        usage = msg.get("usage") or {}
        sess["tokensIn"] += int(usage.get("input_tokens", 0) or 0)
        out_tok = int(usage.get("output_tokens", 0) or 0)
        sess["tokensOut"] += out_tok
        content = msg.get("content")
        if not isinstance(content, list):
            continue
        for b in content:
            if not isinstance(b, dict):
                continue
            bt = b.get("type")
            if bt == "tool_use":
                name = b.get("name", "?")
                title, detail = tool_title(name, b.get("input"))
                res = result_by_id.get(b.get("id"))
                end = None
                status = "ok"
                if res:
                    ro, rb = res
                    rt = parse_ts(ro.get("timestamp"))
                    end = int(rt - t0) if rt is not None else rel
                    if rb.get("is_error"):
                        status = "err"
                        sess["numErrors"] += 1
                steps.append({
                    "i": len(steps), "kind": "tool", "tool": name,
                    "title": title, "detail": detail, "status": status,
                    "startMs": rel, "endMs": end,
                    "durMs": (end - rel) if (end is not None and rel is not None) else None,
                    "tokensOut": out_tok, "sidechain": bool(o.get("isSidechain")),
                    "uuid": o.get("uuid"), "parentUuid": o.get("parentUuid"),
                })
                sess["numSteps"] += 1
                agg = tools.setdefault(name, {"name": name, "count": 0, "errors": 0, "totalMs": 0})
                agg["count"] += 1
                if status == "err":
                    agg["errors"] += 1
                if steps[-1]["durMs"]:
                    agg["totalMs"] += steps[-1]["durMs"]
            elif bt == "thinking":
                steps.append({
                    "i": len(steps), "kind": "think", "tool": None,
                    "title": "thinking", "detail": short(b.get("thinking"), 70),
                    "status": "info", "startMs": rel, "endMs": rel, "durMs": 0,
                    "tokensOut": 0, "sidechain": bool(o.get("isSidechain")),
                    "uuid": o.get("uuid"), "parentUuid": o.get("parentUuid"),
                })
            # assistant "text" blocks are prose, not steps — skipped on purpose

    # user-lens phases: coarse buckets between thinking turns / tool families
    phases = derive_phases(steps, sess["durationMs"])

    out = {
        "session": sess,
        "segments": segments,
        "steps": steps,
        "tools": sorted(tools.values(), key=lambda x: -x["count"]),
        "phases": phases,
        "meta": {
            "source": path,
            "generatedNote": "parsed from Claude session log",
        },
    }
    json.dump(out, sys.stdout, ensure_ascii=False, indent=None)
    sys.stdout.write("\n")


def derive_phases(steps, total):
    """Group consecutive tool steps into a handful of user-facing phases."""
    tool_steps = [s for s in steps if s["kind"] == "tool" and s["startMs"] is not None]
    if not tool_steps:
        return []
    # bucket into up to 6 equal-time phases labeled by dominant tool
    n = min(6, max(1, len(tool_steps)))
    span = max(1, total)
    buckets = [[] for _ in range(n)]
    for s in tool_steps:
        idx = min(n - 1, int(s["startMs"] / span * n))
        buckets[idx].append(s)
    phases = []
    for i, bk in enumerate(buckets):
        if not bk:
            continue
        names = {}
        for s in bk:
            names[s["tool"]] = names.get(s["tool"], 0) + 1
        dom = max(names, key=names.get)
        phases.append({
            "label": dom, "startMs": bk[0]["startMs"],
            "endMs": max(s["endMs"] or s["startMs"] for s in bk),
            "status": "err" if any(s["status"] == "err" for s in bk) else "ok",
        })
    return phases


if __name__ == "__main__":
    main()
