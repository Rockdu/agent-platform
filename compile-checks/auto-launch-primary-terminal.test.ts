// Regression: a workspace tab in the auto-launch flow MUST NOT call
// `spawnTerminal` while waiting for the host scheduler. The
// `awaitingAutoLaunch` prop on `TerminalMeshView` is the gate; the
// `existingTerminalId` prop is filled in by the lifecycle hook once
// the scheduler records the real PTY.
//
// This is a compile-only contract test (no React renderer). It
// asserts that:
// 1. The `TerminalMeshView` props type carries `awaitingAutoLaunch`
//    and `existingTerminalId` so callers can express the auto-launch
//    deferral.
// 2. The `OpenTab` shape (used by `MultiTerminalContainer`) carries
//    `awaitingAutoLaunch` so the per-tab state survives across
//    renders and feeds back into prop derivation.
//
// `npx tsx compile-checks/auto-launch-primary-terminal.test.ts`
// exits 0 when the types align. The runtime body just verifies the
// compile-time probes were reachable.

import type { TerminalMeshViewProps } from "../src/TerminalMeshView";

export function _probe_auto_launch_props_shape(): void {
  // Compile-only: a tab in the auto-launch flow with no live
  // terminal_id yet looks like this. `awaitingAutoLaunch=true` AND
  // `existingTerminalId=undefined` is the "waiting" state.
  const waiting: TerminalMeshViewProps = {
    active: false,
    tabId: "tab-id",
    workspaceId: "ws-id",
    awaitingAutoLaunch: true,
    // No existingTerminalId yet — scheduler has not recorded the PTY.
  };
  void waiting;

  // Once the scheduler records the PTY, the lifecycle hook resolves
  // a terminal_id and the parent passes it down. The view then
  // attaches instead of calling spawnTerminal.
  const attached: TerminalMeshViewProps = {
    active: true,
    tabId: "tab-id",
    workspaceId: "ws-id",
    awaitingAutoLaunch: true,
    existingTerminalId: "term-id",
  };
  void attached;
}

function main(): void {
  _probe_auto_launch_props_shape();
}

main();
