// Pure helper: drop a single tab-id-keyed entry from a per-tab
// `Record<string, T>` state map, returning either a fresh map
// without the entry or the same map reference when the entry
// was already absent.
//
// Used by `closeTab` and `adoptWorkspaceTab` in `App.tsx` to
// clear stale auto-launch errors and focus nonces when a tab id
// is reused for a reopened workspace. The bare-workspace-UUID
// tab id design means closing and reopening the same workspace
// keeps the same `tabId`; without an explicit clear, the next
// `TerminalMeshView` mount inherits the prior process's failure
// banner or stale focus nonce.
//
// Returning the same reference when the entry is absent lets
// React's setState shallow-equality skip a re-render in the
// common no-op case.

export function removeTabKeyedEntry<T>(
  map: Record<string, T>,
  tabId: string,
): Record<string, T> {
  if (!(tabId in map)) return map;
  const next = { ...map };
  delete next[tabId];
  return next;
}
