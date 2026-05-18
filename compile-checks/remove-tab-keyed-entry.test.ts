// Pure-helper compile-check for the per-tab map cleanup the
// App.tsx close/adopt paths use. The tab id is the bare
// workspace UUID, so reopening the same workspace keeps the
// same tabId; without explicit cleanup, the next mount would
// inherit the prior process's auto-launch failure banner or
// stale focus nonce.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { removeTabKeyedEntry } from "../src/remove-tab-keyed-entry";

function fail(label: string, why: string): never {
  console.error(`FAIL ${label}: ${why}`);
  process.exit(1);
}

// 1. Remove an existing entry returns a new map without the key.
{
  const before: Record<string, string> = { "tab-a": "x", "tab-b": "y" };
  const after = removeTabKeyedEntry(before, "tab-a");
  if (after === before) fail("removes existing key", "returned same reference");
  if ("tab-a" in after) fail("removes existing key", "tab-a still present");
  if (after["tab-b"] !== "y") fail("removes existing key", "lost tab-b");
  console.log("PASS removes existing key");
}

// 2. Removing an absent entry returns the same reference
// (lets React's shallow-equality skip a re-render).
{
  const before: Record<string, string> = { "tab-a": "x" };
  const after = removeTabKeyedEntry(before, "tab-missing");
  if (after !== before)
    fail("absent key → same reference", "returned a new object");
  console.log("PASS absent key returns same reference");
}

// 3. Empty map + any key returns the same empty map reference.
{
  const before: Record<string, string> = {};
  const after = removeTabKeyedEntry(before, "tab-a");
  if (after !== before) fail("empty map → same reference", "returned new object");
  console.log("PASS empty map returns same reference");
}

// 4. Removing the only entry yields an empty new map.
{
  const before: Record<string, string> = { "tab-only": "x" };
  const after = removeTabKeyedEntry(before, "tab-only");
  if (after === before) fail("removes only key", "returned same reference");
  if (Object.keys(after).length !== 0)
    fail("removes only key", "expected empty map");
  console.log("PASS removes only entry");
}

// 5. Typed: works for arbitrary value types (boolean, number,
// nested objects) — covers focusNonceByTabId<number> and
// autoLaunchErrorByTabId<AutoLaunchErrorDto> use cases.
{
  const beforeNum: Record<string, number> = { "tab-a": 3, "tab-b": 5 };
  const afterNum = removeTabKeyedEntry(beforeNum, "tab-b");
  if ("tab-b" in afterNum) fail("typed number map", "tab-b still present");
  if (afterNum["tab-a"] !== 3) fail("typed number map", "lost tab-a");
  console.log("PASS typed number map");

  const beforeObj: Record<string, { kind: string }> = {
    "tab-a": { kind: "x" },
  };
  const afterObj = removeTabKeyedEntry(beforeObj, "tab-a");
  if (Object.keys(afterObj).length !== 0)
    fail("typed object map", "expected empty after remove");
  console.log("PASS typed object map");
}

console.log("remove-tab-keyed-entry: all assertions passed");
