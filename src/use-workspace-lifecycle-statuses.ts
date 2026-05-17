// React hook that owns ONE `lifecycle://updated` subscription for an
// entire rail and tracks the latest snapshot per tab id. The parent
// rail uses the returned map to partition rows into Running / Done
// sections and to sort each section by activity timestamp; row
// components stay pure presentational and receive the snapshot as a
// prop.
//
// Bootstrap race: when a rail row is added to the parent, its
// `TerminalMeshView` may still be spawning the underlying PTY, so the
// initial `workspace_lifecycle_snapshot(tabId)` fetch can resolve to
// `null`. The hook re-fetches unresolved tabs with exponential
// backoff (200/400/800/1600 ms, max 5 attempts) until the terminal is
// recorded OR the tab is removed from the input list OR the
// component unmounts. The hook also re-fetches unresolved tabs
// opportunistically whenever an event arrives for a terminal id the
// hook does not yet recognize — that signal is a strong hint the
// registry has populated since the last fetch.

import { useEffect, useRef, useState } from "react";

import {
  subscribeWorkspaceLifecycleUpdates,
  workspaceLifecycleSnapshot,
  type LifecycleUpdateEvent,
  type WorkspaceLifecycleSnapshot,
} from "./terminal-mesh";

export const DEFAULT_LIFECYCLE_SNAPSHOT: WorkspaceLifecycleSnapshot = {
  workspaceId: null,
  tabKind: "Workspace",
  transportKind: "Local",
  status: "Running",
  doneReason: null,
  lastActivityAtUnixMs: 0,
};

/**
 * Drop entries for tab ids that are no longer active. Mutates both
 * maps in place: `knownByTabId` loses the stale tab entry, and
 * `tabIdByTerminalId` loses the corresponding reverse lookup.
 *
 * Exported separately so the prune logic is unit-testable without a
 * React renderer. Without this prune, closing and reopening the same
 * workspace (which reuses the bare-UUID workspaceId as tab id)
 * leaves the old terminal id in `knownByTabId`; the retry loop's
 * "have I resolved this tab yet?" check then short-circuits using
 * the stale id, and subsequent `lifecycle://updated` events for the
 * new terminal go to the unknown-id refetch branch which also skips
 * the tab as already-known.
 */
export function pruneStaleRefs(
  activeTabIds: ReadonlyArray<string>,
  knownByTabId: Map<string, string>,
  tabIdByTerminalId: Map<string, string>,
): void {
  const active = new Set(activeTabIds);
  const stale: Array<[string, string]> = [];
  for (const entry of knownByTabId) {
    if (!active.has(entry[0])) {
      stale.push(entry);
    }
  }
  for (const [tabId, terminalId] of stale) {
    knownByTabId.delete(tabId);
    if (tabIdByTerminalId.get(terminalId) === tabId) {
      tabIdByTerminalId.delete(terminalId);
    }
  }
}

export interface LifecycleStatusesMap {
  snapshotByTabId: Readonly<Record<string, WorkspaceLifecycleSnapshot>>;
  terminalIdByTabId: Readonly<Record<string, string | undefined>>;
}

const RETRY_DELAYS_MS = [200, 400, 800, 1600] as const;

export function useWorkspaceLifecycleStatuses(
  tabIds: ReadonlyArray<string>,
): LifecycleStatusesMap {
  const [snapshotByTabId, setSnapshotByTabId] = useState<
    Record<string, WorkspaceLifecycleSnapshot>
  >({});
  const [terminalIdByTabId, setTerminalIdByTabId] = useState<
    Record<string, string | undefined>
  >({});

  // Mutable maps driving the retry loop and event router. We use refs
  // (not state) so the effect callback always sees the freshest data
  // without re-running. The state setters above keep React in sync
  // for re-render after each mutation.
  const tabIdByTerminalIdRef = useRef<Map<string, string>>(new Map());
  const knownTerminalIdByTabIdRef = useRef<Map<string, string>>(new Map());
  const activeTabIdsRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const timeouts = new Set<ReturnType<typeof setTimeout>>();

    const targetTabIds = tabIds.slice();
    activeTabIdsRef.current = new Set(targetTabIds);

    const sameKeySet = (
      prev: Readonly<Record<string, unknown>>,
      next: ReadonlyArray<string>,
    ): boolean => {
      const prevKeys = Object.keys(prev);
      if (prevKeys.length !== next.length) return false;
      for (const id of next) {
        if (!(id in prev)) return false;
      }
      return true;
    };

    const seedDefaults = () => {
      setSnapshotByTabId((prev) => {
        if (sameKeySet(prev, targetTabIds)) return prev;
        const nextMap: Record<string, WorkspaceLifecycleSnapshot> = {};
        for (const id of targetTabIds) {
          nextMap[id] = prev[id] ?? DEFAULT_LIFECYCLE_SNAPSHOT;
        }
        return nextMap;
      });
      setTerminalIdByTabId((prev) => {
        if (sameKeySet(prev, targetTabIds)) return prev;
        const nextMap: Record<string, string | undefined> = {};
        for (const id of targetTabIds) {
          nextMap[id] = prev[id];
        }
        return nextMap;
      });
    };

    const applyEntry = (
      tabId: string,
      terminalId: string,
      snapshot: WorkspaceLifecycleSnapshot,
    ) => {
      if (cancelled) return;
      if (!activeTabIdsRef.current.has(tabId)) return;
      knownTerminalIdByTabIdRef.current.set(tabId, terminalId);
      tabIdByTerminalIdRef.current.set(terminalId, tabId);
      setSnapshotByTabId((prev) => ({ ...prev, [tabId]: snapshot }));
      setTerminalIdByTabId((prev) => ({ ...prev, [tabId]: terminalId }));
    };

    const fetchWithRetry = (tabId: string, attempt = 0) => {
      if (cancelled) return;
      if (!activeTabIdsRef.current.has(tabId)) return;
      void workspaceLifecycleSnapshot(tabId).then((entry) => {
        if (cancelled) return;
        if (!activeTabIdsRef.current.has(tabId)) return;
        if (entry) {
          applyEntry(tabId, entry.terminalId, entry.snapshot);
          return;
        }
        if (knownTerminalIdByTabIdRef.current.has(tabId)) return;
        if (attempt >= RETRY_DELAYS_MS.length) return;
        const delay = RETRY_DELAYS_MS[attempt];
        const handle = setTimeout(() => {
          timeouts.delete(handle);
          fetchWithRetry(tabId, attempt + 1);
        }, delay);
        timeouts.add(handle);
      });
    };

    const refetchUnresolved = () => {
      for (const id of activeTabIdsRef.current) {
        if (!knownTerminalIdByTabIdRef.current.has(id)) {
          fetchWithRetry(id, 0);
        }
      }
    };

    seedDefaults();
    // Drop ref entries for tabs that were removed since the previous
    // effect cycle. Without this, a closed-then-reopened workspace
    // (same bare-UUID workspaceId reused as tab id) keeps the old
    // terminal id around and the retry/refetch paths short-circuit
    // on the stale entry, leaving the reopened row stuck on the
    // default snapshot.
    pruneStaleRefs(
      targetTabIds,
      knownTerminalIdByTabIdRef.current,
      tabIdByTerminalIdRef.current,
    );

    // Install the listener BEFORE the initial fetch so a Done event
    // that fires between the fetch and the listener installation is
    // not lost.
    void subscribeWorkspaceLifecycleUpdates((event: LifecycleUpdateEvent) => {
      if (cancelled) return;
      const tabId = tabIdByTerminalIdRef.current.get(event.terminalId);
      if (tabId !== undefined) {
        if (!activeTabIdsRef.current.has(tabId)) return;
        setSnapshotByTabId((prev) => ({ ...prev, [tabId]: event.snapshot }));
        return;
      }
      // An event for a terminal id we do not yet recognize: the
      // registry has likely populated since our last fetch, so retry
      // every still-unresolved tab.
      refetchUnresolved();
    }).then((fn) => {
      if (cancelled) {
        fn();
        return;
      }
      unlisten = fn;
    });

    for (const id of targetTabIds) {
      fetchWithRetry(id, 0);
    }

    return () => {
      cancelled = true;
      for (const t of timeouts) clearTimeout(t);
      timeouts.clear();
      if (unlisten) unlisten();
    };
  }, [tabIds]);

  return { snapshotByTabId, terminalIdByTabId };
}
