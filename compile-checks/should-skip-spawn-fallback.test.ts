// Pure-helper compile-check for the predicate
// `TerminalMeshView` uses to decide whether to short-circuit
// its setup effect before calling `spawnTerminal`. The bug this
// fix targets: when an auto-launch failed, prior rounds cleared
// `awaitingAutoLaunch` so the existing waiting placeholder
// would exit; the effect then fell through to `spawnTerminal`
// and silently created a fallback shell next to the error
// banner. The new predicate also short-circuits on
// `autoLaunchError != null`, so the error-state pane stays put.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { shouldSkipSpawnFallback } from "../src/should-skip-spawn-fallback";
import type { AutoLaunchErrorDto } from "../src/workspaces";

function assertEq(label: string, actual: boolean, expected: boolean): void {
  if (actual !== expected) {
    console.error(
      `FAIL ${label}: expected ${expected} but got ${actual}`,
    );
    process.exit(1);
  }
  console.log(`PASS ${label}`);
}

const stubError: AutoLaunchErrorDto = {
  kind: "asyncSpawnFailed",
  transportKind: "Ssh",
  doneReason: "Disconnected",
};

// 1. Awaiting + no resolved id → skip (the original waiting-
// state contract that existed before the fix).
assertEq(
  "awaiting + no id → skip",
  shouldSkipSpawnFallback({
    awaitingAutoLaunch: true,
    autoLaunchError: null,
    existingTerminalId: undefined,
  }),
  true,
);

// 2. Awaiting + resolved id → do NOT skip (attach path takes
// over — the scheduler resolved a real terminal, view re-runs
// and subscribes).
assertEq(
  "awaiting + resolved id → run (attach path)",
  shouldSkipSpawnFallback({
    awaitingAutoLaunch: true,
    autoLaunchError: null,
    existingTerminalId: "term-1",
  }),
  false,
);

// 3. Error installed + no id → skip (the new short-circuit).
// Banner is the source of truth for the failed launch; no
// fallback spawn.
assertEq(
  "error + no id → skip (new short-circuit)",
  shouldSkipSpawnFallback({
    awaitingAutoLaunch: false,
    autoLaunchError: stubError,
    existingTerminalId: undefined,
  }),
  true,
);

// 4. Error + resolved id → run (attach). Even when an error
// was previously installed, a real id means the scheduler
// eventually delivered (or a re-run resolved); attach to it.
assertEq(
  "error + resolved id → run (attach wins)",
  shouldSkipSpawnFallback({
    awaitingAutoLaunch: false,
    autoLaunchError: stubError,
    existingTerminalId: "term-1",
  }),
  false,
);

// 5. Neither + no id → run (the original fallback spawn path
// for normal workspace / transient tabs).
assertEq(
  "no awaiting + no error + no id → run (regular spawn)",
  shouldSkipSpawnFallback({
    awaitingAutoLaunch: false,
    autoLaunchError: null,
    existingTerminalId: undefined,
  }),
  false,
);

// 6. Undefined-everything → run (defensive: same shape as 5).
assertEq(
  "all undefined → run (regular spawn)",
  shouldSkipSpawnFallback({}),
  false,
);

// 7. Awaiting AND error at the same time + no id → skip. (Real
// case: user enqueued, the catch arm fired synchronously and
// installed both the error and cleared awaiting; harness pins
// the AND-or composition so both flags still short-circuit.)
assertEq(
  "awaiting + error + no id → skip",
  shouldSkipSpawnFallback({
    awaitingAutoLaunch: true,
    autoLaunchError: stubError,
    existingTerminalId: undefined,
  }),
  true,
);

console.log("should-skip-spawn-fallback: all assertions passed");
