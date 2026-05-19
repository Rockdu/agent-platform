// Executable regression for the queued-launch → real-terminal_id
// transition. The historical bug: synthesizing a terminal_id from
// the tab_id for the pending-launch placeholder poisoned the
// lifecycle hook — the synthetic id ended up in the resolved
// tab→terminal map, so the later real `lifecycle://updated` event
// was treated as "already known" and the parent kept passing the
// synthetic id down to TerminalMeshView.
//
// The current wire shape carries placeholders as
// `terminalId: null + tabId: <id>`, and the hook's `applyEntry`
// helper updates the snapshot map only for those events. This
// harness replays the placeholder→real sequence against the
// exported pure helper and asserts:
//
//   * After the placeholder event, snapshotByTabId[tabId] reflects
//     pendingLaunch=true AND terminalIdByTabId[tabId] is undefined.
//   * After the real event, snapshotByTabId[tabId] reflects the
//     live snapshot AND terminalIdByTabId[tabId] is the real id.
//
// `npx tsx compile-checks/auto-launch-attach-on-terminal-id.test.ts`
// exits 0 when both asserts pass; it throws on the first failure.

import {
  applyLifecycleEntry,
  type LifecycleApplyState,
} from "../src/use-workspace-lifecycle-statuses";
import type { WorkspaceLifecycleSnapshot } from "../src/terminal-mesh";

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    throw new Error(`auto-launch attach regression failed: ${msg}`);
  }
}

function freshState(): LifecycleApplyState {
  return {
    snapshotByTabId: {},
    terminalIdByTabId: {},
    knownByTabId: new Map(),
    tabIdByTerminalId: new Map(),
  };
}

const TAB_ID = "workspace-uuid-1";
const REAL_TERMINAL_ID = "real-terminal-uuid-2";

function placeholderSnapshot(): WorkspaceLifecycleSnapshot {
  return {
    workspaceId: TAB_ID,
    tabKind: "Workspace",
    transportKind: "Local",
    status: "Running",
    doneReason: null,
    lastActivityAtUnixMs: 1_000,
    pendingLaunch: true,
    promptVisible: false,
  };
}

function realSnapshot(): WorkspaceLifecycleSnapshot {
  return {
    workspaceId: TAB_ID,
    tabKind: "Workspace",
    transportKind: "Local",
    status: "Running",
    doneReason: null,
    lastActivityAtUnixMs: 2_000,
    pendingLaunch: false,
    promptVisible: false,
  };
}

export function runPlaceholderThenRealTransition(): void {
  const state = freshState();

  // Placeholder event arrives (terminalId=null, tabId set).
  // Snapshot must update; terminal-id maps must NOT be populated.
  applyLifecycleEntry(TAB_ID, null, placeholderSnapshot(), state);
  assert(
    state.snapshotByTabId[TAB_ID]?.pendingLaunch === true,
    "placeholder: snapshot must surface pendingLaunch=true",
  );
  assert(
    state.terminalIdByTabId[TAB_ID] === undefined,
    "placeholder: must NOT populate terminalIdByTabId",
  );
  assert(
    !state.knownByTabId.has(TAB_ID),
    "placeholder: must NOT populate knownByTabId",
  );
  assert(
    state.tabIdByTerminalId.size === 0,
    "placeholder: must NOT populate tabIdByTerminalId",
  );

  // Real-terminal event arrives. Snapshot updates AND terminal-id
  // maps are populated with the REAL id (not a stale placeholder).
  applyLifecycleEntry(TAB_ID, REAL_TERMINAL_ID, realSnapshot(), state);
  assert(
    state.snapshotByTabId[TAB_ID]?.pendingLaunch === false,
    "live: real snapshot must clear pendingLaunch",
  );
  assert(
    state.terminalIdByTabId[TAB_ID] === REAL_TERMINAL_ID,
    `live: terminalIdByTabId must hold the real id, got ${state.terminalIdByTabId[TAB_ID]}`,
  );
  assert(
    state.knownByTabId.get(TAB_ID) === REAL_TERMINAL_ID,
    "live: knownByTabId must hold the real id",
  );
  assert(
    state.tabIdByTerminalId.get(REAL_TERMINAL_ID) === TAB_ID,
    "live: reverse map must point at the tab",
  );
  assert(
    state.tabIdByTerminalId.size === 1,
    "live: reverse map must hold exactly one entry (no synthetic id leaked)",
  );
}

declare const require: { main?: unknown } | undefined;
declare const module: unknown;
if (typeof require !== "undefined" && typeof module !== "undefined" && require.main === module) {
  runPlaceholderThenRealTransition();
}
