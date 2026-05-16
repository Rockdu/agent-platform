// Real frontend component for the example-notes plugin.
//
// Validates that the plugin contract loads a manifest-declared frontend
// component into the host shell without any host source edit. Trivially
// stateful so a runtime probe can confirm the actual code (not a stub) is
// rendered.

import { useState } from "react";
import type { CreateNoteArgs } from "../types";

const FIXTURE_BANNER = "example-notes-plugin-real-frontend-loaded";

export default function NotesPanel() {
  const [draft, setDraft] = useState("");
  const [notes, setNotes] = useState<string[]>([]);

  function submit() {
    const trimmed = draft.trim();
    if (!trimmed) return;
    // Round-3 stub: command dispatch lands with task4; this exists only to
    // exercise the typed wrapper type-check path at compile time, not at
    // runtime.
    const args: CreateNoteArgs = { body: trimmed };
    setNotes((prev) => [...prev, args.body]);
    setDraft("");
  }

  return (
    <section className="placeholder" data-fixture={FIXTURE_BANNER}>
      <h2>Notes (示例插件)</h2>
      <p>
        本面板由 <code>plugins/example-notes/frontend/index.tsx</code>
        直接渲染，证明 manifest 声明的 frontend 真的被 host 加载，没有改 host 源码。
      </p>
      <div className="notes-input">
        <input
          aria-label="note draft"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="写一条笔记…"
        />
        <button type="button" onClick={submit}>
          添加
        </button>
      </div>
      <ul className="notes-list">
        {notes.length === 0 && <li className="notes-list__empty">（空）</li>}
        {notes.map((n, i) => (
          <li key={i}>{n}</li>
        ))}
      </ul>
    </section>
  );
}
