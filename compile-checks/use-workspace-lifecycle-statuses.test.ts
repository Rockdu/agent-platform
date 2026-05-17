// Regression coverage for the `pruneStaleRefs` helper that
// `useWorkspaceLifecycleStatuses` calls on every effect cycle to drop
// terminal-id refs for tab ids that are no longer active. Without
// this helper, a closed-then-reopened workspace (which reuses the
// same `tab-${workspaceId}` tab id) keeps the old terminal id in the
// known-by-tab map; the retry loop's "already known" check then
// short-circuits using the stale id, and the row stays stuck on the
// fallback Running snapshot.
//
// No runtime test framework is wired up for the frontend in this
// project yet, so this file exposes the assertions as a callable
// function. tsc compiles the file as part of `npm run build`, which
// proves the helper's exported shape is stable. To execute the
// assertions, run `npx tsx compile-checks/use-workspace-lifecycle-
// statuses.test.ts`. The function throws on the first failed
// assertion; a successful invocation prints no output.

import { pruneStaleRefs } from "../src/use-workspace-lifecycle-statuses";

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    throw new Error(`pruneStaleRefs assertion failed: ${msg}`);
  }
}

export function runAllPruneAssertions(): void {
  // Scenario 1: a previously known tab is no longer active. Both the
  // forward and reverse entries must be deleted.
  {
    const known = new Map<string, string>([["tab-w1", "old"]]);
    const reverse = new Map<string, string>([["old", "tab-w1"]]);
    pruneStaleRefs([], known, reverse);
    assert(
      !known.has("tab-w1"),
      "Scenario 1: stale tab-w1 must be removed from knownByTabId",
    );
    assert(
      !reverse.has("old"),
      "Scenario 1: stale `old` terminal id must be removed from tabIdByTerminalId",
    );
  }

  // Scenario 2: a still-active tab is preserved on both sides.
  {
    const known = new Map<string, string>([["tab-w1", "old"]]);
    const reverse = new Map<string, string>([["old", "tab-w1"]]);
    pruneStaleRefs(["tab-w1"], known, reverse);
    assert(
      known.get("tab-w1") === "old",
      "Scenario 2: active tab-w1 must retain its terminal id mapping",
    );
    assert(
      reverse.get("old") === "tab-w1",
      "Scenario 2: active reverse mapping must be retained",
    );
  }

  // Scenario 3: mixed — stale tab gets pruned, active tab kept, and
  // the helper does NOT insert entries for newly active tabs that
  // were never known (insertion is the fetch path's job, not the
  // prune helper's).
  {
    const known = new Map<string, string>([
      ["tab-w1", "old1"],
      ["tab-w2", "old2"],
    ]);
    const reverse = new Map<string, string>([
      ["old1", "tab-w1"],
      ["old2", "tab-w2"],
    ]);
    pruneStaleRefs(["tab-w1", "tab-w3"], known, reverse);
    assert(
      known.get("tab-w1") === "old1",
      "Scenario 3: tab-w1 is still active so its mapping survives",
    );
    assert(
      !known.has("tab-w2"),
      "Scenario 3: tab-w2 is no longer active and must be dropped",
    );
    assert(
      !known.has("tab-w3"),
      "Scenario 3: tab-w3 is newly active but the helper must not insert it",
    );
    assert(
      reverse.get("old1") === "tab-w1",
      "Scenario 3: tab-w1 reverse mapping survives",
    );
    assert(
      !reverse.has("old2"),
      "Scenario 3: tab-w2 reverse mapping is removed",
    );
  }

  // Scenario 4: if the reverse map's value points at a different
  // tab id than the one being pruned (rare race where a terminal id
  // was rebound), the helper leaves the reverse entry alone. This
  // guards against accidentally clearing a fresh terminal id that
  // already replaced the stale one.
  {
    const known = new Map<string, string>([["tab-w1", "shared-terminal"]]);
    const reverse = new Map<string, string>([
      ["shared-terminal", "tab-w2"], // rebound to a different tab
    ]);
    pruneStaleRefs([], known, reverse);
    assert(
      !known.has("tab-w1"),
      "Scenario 4: stale tab-w1 forward entry is removed",
    );
    assert(
      reverse.get("shared-terminal") === "tab-w2",
      "Scenario 4: reverse entry pointing at a different tab is preserved",
    );
  }
}

// Self-invoke when this file is run directly via `node` or `tsx`,
// but stay inert when imported (or compiled-only) so the tsc build
// remains side-effect free.
declare const require: { main?: unknown } | undefined;
declare const module: unknown;
if (typeof require !== "undefined" && typeof module !== "undefined" && require.main === module) {
  runAllPruneAssertions();
}
