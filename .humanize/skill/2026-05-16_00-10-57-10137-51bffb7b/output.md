**AGREE**

Round-1 required changes are mostly applied correctly:

- Typed IPC from Rust SoT is now explicit.
- Per-plugin SQLite files replaced shared/attached namespace ambiguity.
- Permission gating is mandatory, including in lower bound.
- Zotero now has three distinct modes: Live / SQLite-ro / cached snapshot.
- Gmail scope contradiction is resolved: read + modify, no send/reply.
- Stderr-burst was removed from default needs-attention triggers.
- Stronghold is now the primary secret store.
- `analyze` tasks now have concrete spec deliverables.

**DISAGREE**

Two v2 additions still leave implementation ambiguity:

- “No host router/tab-switch edits” is not fully satisfied unless the frontend plugin registry/component import map is also generated. v2 only clearly generates Rust registry + TS command wrappers.
- IPC permission gating needs a caller-identity model. A single WebView can import any generated wrapper unless the host can authenticate which plugin/component initiated the call.

**REQUIRED_CHANGES**

1. Reconcile Acceptance Criteria vs Lower Bound.

   The Lower Bound currently permits outcomes that fail the ACs:
   - Terminal: lower bound says `>=2` PTYs and native OR tray; AC requires `>=4` and native + tray.
   - Gmail: lower bound says `>=1` account read-only; AC requires at least two accounts and read/modify behavior in scope.
   - Papers: lower bound says one Zotero source mode; AC requires three-mode Zotero behavior.

   Fix by either aligning Lower Bound to the ACs, or explicitly labeling it as “prototype fallback, not MVP acceptance.”

2. Add generated frontend plugin registry to AC-1 / tasks.

   To satisfy “drop plugin directory without host router/tab-switch edits,” require generation of a frontend registry such as `generated/plugin-tabs.ts` with lazy imports/components from each plugin manifest. Otherwise React routing still risks becoming a manual switch table.

3. Define enforceable IPC caller identity for permission gating.

   Add a required design choice: per-plugin WebViews, host-issued plugin instance tokens, scoped capability channels, or another non-spoofable mechanism. Generated TS wrappers alone do not prove the caller plugin identity.

**OPTIONAL_IMPROVEMENTS**

- Move `docs/specs/terminal-events.md` before terminal implementation or at least before notification implementation; current ordering writes the taxonomy after the PTY frontend.
- Add an explicit generated Rust IPC dispatcher task, not just “registry table,” so Tauri command registration and permission gating are clearly covered.
- Clarify whether Gmail delete is included. DEC-4 says `gmail.modify` allows delete, but Upper Bound lists label/archive/mark-read only.

**UNRESOLVED**

Pending decisions list is reasonable and unchanged. The most important to resolve before implementation are DEC-3/DEC-4 Gmail scope, DEC-10 local DB privacy posture, and DEC-12 retention policy.

Further required changes: yes, the three above.
