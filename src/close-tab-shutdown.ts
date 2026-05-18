// Pure helper used by App.tsx's `closeTab` to decide whether to
// issue a `shutdownTerminal` before dropping a tab from state.
//
// The scheduler creates the PTY for auto-launched workspaces and
// publishes its id via the lifecycle hook. `TerminalMeshView`
// receives that id as `existingTerminalId` and — by design —
// skips `terminal_shutdown` in its cleanup path because the id
// is externally owned. Without this helper / explicit shutdown
// at the close site, the auto-launched PTY + child process
// would outlive the tab.
//
// Returns the scheduler-resolved terminal id when the tab owns
// its auto-launched terminal AND a real id has been published;
// `null` otherwise (either the tab spawned its own PTY — in
// which case `TerminalMeshView` cleanup handles shutdown — or
// the auto-launch never resolved). Lives in its own module so
// the tsx compile-check can import it without dragging xterm
// CSS imports from `App.tsx` through the harness.

export function resolveAutoLaunchTerminalToShutdown(
  tab: { ownsAutoLaunchTerminal: boolean; tabId: string },
  terminalIdByTabId: Record<string, string | undefined>,
): string | null {
  if (!tab.ownsAutoLaunchTerminal) return null;
  const resolved = terminalIdByTabId[tab.tabId];
  return resolved ?? null;
}
