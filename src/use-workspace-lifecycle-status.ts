// React hook that bootstraps a workspace tab's lifecycle snapshot
// from the host and subscribes to live updates over the
// `lifecycle://updated` event topic. The rail row uses the returned
// snapshot to drive its transport-kind icon and status badge.
//
// Filtering: the host emits one `lifecycle://updated` envelope per
// snapshot mutation across all tabs, so each hook caches its own
// terminal id (resolved from the initial fetch) and ignores events
// for other terminals.

import { useEffect, useRef, useState } from "react";

import {
  LIFECYCLE_UPDATED_TOPIC,
  subscribeWorkspaceLifecycleUpdates,
  workspaceLifecycleSnapshot,
  type LifecycleUpdateEvent,
  type WorkspaceLifecycleSnapshot,
} from "./terminal-mesh";

// Suppress unused-import warnings while keeping the constant visible
// to consumers that need to subscribe directly (kept in the public
// API surface even though the hook handles subscription internally).
void LIFECYCLE_UPDATED_TOPIC;

export const DEFAULT_LIFECYCLE_SNAPSHOT: WorkspaceLifecycleSnapshot = {
  workspaceId: null,
  tabKind: "Workspace",
  transportKind: "Local",
  status: "Running",
  doneReason: null,
  lastActivityAtUnixMs: 0,
};

export function useWorkspaceLifecycleStatus(
  tabId: string,
): WorkspaceLifecycleSnapshot {
  const [snapshot, setSnapshot] = useState<WorkspaceLifecycleSnapshot>(
    DEFAULT_LIFECYCLE_SNAPSHOT,
  );
  // Cached so the subscriber callback can filter events to this tab's
  // terminal id without re-fetching every event.
  const terminalIdRef = useRef<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    // Reset on tab-id change so stale state from a previous tab
    // identity does not leak through.
    terminalIdRef.current = null;
    setSnapshot(DEFAULT_LIFECYCLE_SNAPSHOT);

    void workspaceLifecycleSnapshot(tabId).then((entry) => {
      if (cancelled || !entry) return;
      terminalIdRef.current = entry.terminalId;
      setSnapshot(entry.snapshot);
    });

    void subscribeWorkspaceLifecycleUpdates(
      (event: LifecycleUpdateEvent) => {
        if (cancelled) return;
        const known = terminalIdRef.current;
        if (known === null) {
          // Initial fetch hasn't resolved yet. Skip — the fetch
          // resolution will produce the up-to-date snapshot below.
          return;
        }
        if (event.terminalId !== known) return;
        setSnapshot(event.snapshot);
      },
    ).then((fn) => {
      if (cancelled) {
        fn();
        return;
      }
      unlisten = fn;
    });

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [tabId]);

  return snapshot;
}
