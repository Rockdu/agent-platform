// Real frontend component for the example-notes plugin.
//
// Validates that the plugin contract loads a manifest-declared frontend
// component into the host shell without any host source edit. Also proves
// the React lifecycle round-trip: the host wraps this component in
// PluginRoot, mints a capability via mountPlugin, and exposes it via
// usePluginCapabilityValue(). Closing the tab triggers unmountPlugin.

import { useState } from "react";
import type { CreateNoteArgs } from "../types";
import { usePluginCapabilityValue } from "../../../src/plugin-lifecycle";

const FIXTURE_BANNER = "example-notes-plugin-real-frontend-loaded";

export default function NotesPanel() {
  const capability = usePluginCapabilityValue();
  const [draft, setDraft] = useState("");
  const [notes, setNotes] = useState<string[]>([]);

  function submit() {
    const trimmed = draft.trim();
    if (!trimmed) return;
    // Wire-only proof for the typed wrapper. Real dispatch to a sidecar
    // lands with task11; the host dispatcher currently returns
    // no_sidecar_wired so we keep the local note list working until then.
    const args: CreateNoteArgs = { body: trimmed };
    setNotes((prev) => [...prev, args.body]);
    setDraft("");
  }

  // Show only `cap_v1.<first-8-of-mount-uuid>…` from the envelope.
  // The full mount UUID is reachable via `mount_id` (the non-secret mount
  // identity) below; this prefix is just human-recognizable proof that the
  // handle is a real cap_v1.* envelope. Nonce body is never displayed.
  const handleEnvelopePrefix = (() => {
    const parts = capability.capability.split(".");
    if (parts.length < 2 || parts[0] !== "cap_v1") return "cap_v1.…";
    return `cap_v1.${parts[1].slice(0, 8)}…`;
  })();

  return (
    <section className="placeholder" data-fixture={FIXTURE_BANNER}>
      <h2>Notes (示例插件)</h2>
      <p>
        本面板由 <code>plugins/example-notes/frontend/index.tsx</code>
        直接渲染，证明 manifest 声明的 frontend 真的被 host 加载，没有改 host 源码。
      </p>
      <dl className="plugin-capability-summary">
        <dt>mount_id</dt>
        <dd>
          <code>{capability.mountId}</code>
        </dd>
        <dt>handle 前缀</dt>
        <dd>
          <code>{handleEnvelopePrefix}</code>
        </dd>
        <dt>generation</dt>
        <dd>
          <code>{capability.generation}</code>
        </dd>
      </dl>
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
