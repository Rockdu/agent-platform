// Pure-helper compile-check for `closeTab`'s
// "should-we-shutdown-the-scheduler-owned-terminal" decision.
// The bug this fix targets: scheduler-created terminals were
// passed to `TerminalMeshView` via `existingTerminalId`, which
// the view treats as externally owned and skips
// `terminal_shutdown` for. `closeTab` previously only called
// `closeWorkspace`, leaving the PTY + child process alive past
// the tab close.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { resolveAutoLaunchTerminalToShutdown } from "../src/close-tab-shutdown";

function assertEq<T>(label: string, actual: T, expected: T): void {
  if (actual !== expected) {
    console.error(
      `FAIL ${label}: expected ${JSON.stringify(expected)} but got ${JSON.stringify(actual)}`,
    );
    process.exit(1);
  }
  console.log(`PASS ${label}`);
}

// 1. Auto-launched tab whose terminal id has been resolved by
// the scheduler → return the id so closeTab calls shutdown.
assertEq(
  "owns-auto-launched + resolved id → returns the id",
  resolveAutoLaunchTerminalToShutdown(
    { ownsAutoLaunchTerminal: true, tabId: "tab-a" },
    { "tab-a": "term-1" },
  ),
  "term-1",
);

// 2. Auto-launched tab whose terminal id has NOT been resolved
// yet (auto-launch failed before recording, queued without
// drain, etc.) → null. closeTab should still call closeWorkspace,
// but the placeholder cleanup is handled elsewhere.
assertEq(
  "owns-auto-launched + no resolved id → returns null",
  resolveAutoLaunchTerminalToShutdown(
    { ownsAutoLaunchTerminal: true, tabId: "tab-b" },
    { "tab-a": "term-1" },
  ),
  null,
);

// 3. Tab that does NOT own its terminal (the view spawned its
// own PTY) → null. The view's cleanup handles shutdown; closing
// here would double-shutdown a foreign PTY.
assertEq(
  "no ownership claim → returns null even with resolved id",
  resolveAutoLaunchTerminalToShutdown(
    { ownsAutoLaunchTerminal: false, tabId: "tab-c" },
    { "tab-c": "term-3" },
  ),
  null,
);

// 4. Defensive: empty map.
assertEq(
  "owns-auto-launched + empty id map → null",
  resolveAutoLaunchTerminalToShutdown(
    { ownsAutoLaunchTerminal: true, tabId: "tab-d" },
    {},
  ),
  null,
);

console.log("close-tab-shutdown: all assertions passed");
