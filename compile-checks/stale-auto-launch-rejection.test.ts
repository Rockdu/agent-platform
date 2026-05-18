// Pure-helper compile-check for the stale-rejection guard the
// App.tsx fire-and-forget `requestWorkspaceAutoLaunch` `.catch`
// arm uses. Workspace tabs reuse the workspace UUID as `tabId`,
// so close-then-reopen-then-late-reject would otherwise corrupt
// the new tab's state.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { isStaleAutoLaunchRejection } from "../src/stale-auto-launch-rejection";

function fail(label: string, why: string): never {
  console.error(`FAIL ${label}: ${why}`);
  process.exit(1);
}

interface T {
  tabId: string;
  openGeneration: number;
}

// 1. Matching tab + matching generation → NOT stale.
{
  const tabs: T[] = [{ tabId: "ws-1", openGeneration: 7 }];
  const stale = isStaleAutoLaunchRejection({
    tabs,
    tabId: "ws-1",
    capturedGeneration: 7,
  });
  if (stale)
    fail("matching gen → not stale", "should apply the rejection");
  console.log("PASS matching gen accepts rejection");
}

// 2. Matching tab + mismatched generation → STALE.
// This is the close-then-reopen scenario: the user closed the
// workspace mid-launch (gen=7) and reopened (gen=8); the gen=7
// rejection MUST NOT clobber the gen=8 tab's state.
{
  const tabs: T[] = [{ tabId: "ws-1", openGeneration: 8 }];
  const stale = isStaleAutoLaunchRejection({
    tabs,
    tabId: "ws-1",
    capturedGeneration: 7,
  });
  if (!stale)
    fail(
      "mismatched gen → stale",
      "older incarnation's rejection must be ignored",
    );
  console.log("PASS mismatched gen drops rejection");
}

// 3. No matching tab (tab was closed and NOT reopened) → STALE.
// There is no tab to apply state to; the catch handler should be
// a no-op.
{
  const tabs: T[] = [{ tabId: "ws-other", openGeneration: 1 }];
  const stale = isStaleAutoLaunchRejection({
    tabs,
    tabId: "ws-1",
    capturedGeneration: 1,
  });
  if (!stale)
    fail("no matching tab → stale", "no target tab; must drop");
  console.log("PASS missing tab drops rejection");
}

// 4. Empty tabs array → STALE.
{
  const stale = isStaleAutoLaunchRejection({
    tabs: [],
    tabId: "ws-1",
    capturedGeneration: 1,
  });
  if (!stale) fail("empty tabs → stale", "no tabs to apply to");
  console.log("PASS empty tabs drops rejection");
}

// 5. Multiple tabs, match the right one by tabId.
{
  const tabs: T[] = [
    { tabId: "ws-a", openGeneration: 3 },
    { tabId: "ws-b", openGeneration: 5 },
    { tabId: "ws-c", openGeneration: 9 },
  ];
  if (
    isStaleAutoLaunchRejection({
      tabs,
      tabId: "ws-b",
      capturedGeneration: 5,
    })
  )
    fail("multi-tab match", "ws-b at gen=5 must apply");
  if (
    !isStaleAutoLaunchRejection({
      tabs,
      tabId: "ws-b",
      capturedGeneration: 4,
    })
  )
    fail("multi-tab gen mismatch", "ws-b at gen=4 must be stale");
  console.log("PASS multi-tab tabId discriminates correctly");
}

// 6. Generation === 0 is a valid value (first-ever adoption).
{
  const tabs: T[] = [{ tabId: "ws-zero", openGeneration: 0 }];
  if (
    isStaleAutoLaunchRejection({
      tabs,
      tabId: "ws-zero",
      capturedGeneration: 0,
    })
  )
    fail("gen=0 valid", "first adoption (gen=0) must accept");
  console.log("PASS gen=0 is a valid generation");
}

console.log("stale-auto-launch-rejection: all assertions passed");
