// Pure helper: detect the "auto-launch failed asynchronously but
// the pane is still showing 等待 claude 启动…" condition and
// synthesize the AutoLaunchErrorDto that should be installed into
// `autoLaunchErrorByTabId` so `TerminalMeshView` exits the
// waiting branch.
//
// The condition: the tab is awaiting auto-launch, its retained
// lifecycle snapshot has transitioned to `Done` (the backend's
// `surface_auto_launch_async_failure` path emits this), AND no
// real terminal id has resolved through `terminalIdByTabId`
// (the lifecycle envelope used `terminalId: null` because no
// PTY was ever recorded for this tab).
//
// Returns the synthesized error DTO when the condition is met,
// or `null` otherwise. Lives in its own module so the tsx
// harness can import it without dragging xterm CSS through.

import type { AutoLaunchErrorDto } from "./workspaces";
import type {
  DoneReason,
  WorkspaceLifecycleSnapshot,
} from "./terminal-mesh";

function formatDoneReason(reason: DoneReason | null | undefined): string {
  if (reason === null || reason === undefined) return "Done";
  switch (reason.kind) {
    case "CleanCompletion":
      return "CleanCompletion";
    case "NonZeroExit":
      return `NonZeroExit(${reason.code})`;
    case "Disconnected":
      return "Disconnected";
    case "TaskComplete":
      return "TaskComplete";
  }
}

export function synthesizeStuckAutoLaunchError(
  tab: { tabId: string; awaitingAutoLaunch: boolean },
  snapshot: WorkspaceLifecycleSnapshot | undefined,
  resolvedTerminalId: string | undefined,
): AutoLaunchErrorDto | null {
  if (!tab.awaitingAutoLaunch) return null;
  if (resolvedTerminalId !== undefined) return null;
  if (snapshot === undefined) return null;
  if (snapshot.status !== "Done") return null;
  return {
    kind: "asyncSpawnFailed",
    transportKind: String(snapshot.transportKind),
    doneReason: formatDoneReason(snapshot.doneReason),
  };
}
