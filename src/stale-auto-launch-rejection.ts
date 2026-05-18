// Pure helper: detect a stale fire-and-forget
// `requestWorkspaceAutoLaunch` rejection from a prior tab
// incarnation.
//
// Workspace tabs reuse the workspace UUID as `tabId`, so a user
// who closes a workspace mid-auto-launch and quickly reopens the
// same workspace produces a new tab with the SAME `tabId` but a
// DISTINCT `openGeneration`. A late rejection from the first
// incarnation would otherwise write `autoLaunchErrorByTabId[tabId]`
// and clear `awaitingAutoLaunch` on the NEW tab — hiding the new
// scheduler launch and showing a stale error banner.
//
// The fire-and-forget closure captures the `openGeneration` at
// fire time; before applying state updates the `.catch` arm calls
// this helper with the latest `tabs` array. Returns `true` when
// the rejection should be IGNORED.

interface TabIncarnation {
  tabId: string;
  openGeneration: number;
}

export function isStaleAutoLaunchRejection<T extends TabIncarnation>(args: {
  tabs: ReadonlyArray<T>;
  tabId: string;
  capturedGeneration: number;
}): boolean {
  const { tabs, tabId, capturedGeneration } = args;
  const current = tabs.find((t) => t.tabId === tabId);
  if (!current) return true;
  return current.openGeneration !== capturedGeneration;
}
