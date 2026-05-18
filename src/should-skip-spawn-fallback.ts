// Pure helper: decide whether `TerminalMeshView`'s setup effect
// should short-circuit BEFORE calling `spawnTerminal`. Three
// short-circuit conditions, all gated on "no existing terminal
// id has resolved yet":
//
// 1. `awaitingAutoLaunch === true`: the scheduler is still
//    working on the launch; the live terminal id will arrive
//    via the lifecycle subscription, and we must NOT spawn a
//    second PTY in the meantime.
//
// 2. `autoLaunchError != null`: the scheduler-owned launch
//    failed (synchronous reject OR async Done placeholder, both
//    surface as a typed error DTO). The error banner is the
//    intended pane content for the failed launch; spawning a
//    fallback shell/remote terminal would silently undo the
//    error-state semantics.
//
// 3. `existingTerminalId` set: the attach path takes over — the
//    scheduler resolved a real PTY for this tab. This is the
//    explicit exception to both short-circuits above.
//
// Returns true when the spawn effect should bail out. Returns
// false when the regular spawnTerminal path should run (the
// only "neither awaiting nor errored nor attaching" case).
//
// Extracted into its own module so the tsx compile-check
// harness can pin the predicate's truth table without dragging
// React or xterm CSS imports through the test.

import type { AutoLaunchErrorDto } from "./workspaces";

export function shouldSkipSpawnFallback(args: {
  awaitingAutoLaunch?: boolean;
  autoLaunchError?: AutoLaunchErrorDto | null;
  existingTerminalId?: string;
}): boolean {
  // The attach path always wins — even when the awaiting flag
  // or an error is still installed, a resolved real terminal
  // id means the scheduler eventually delivered, and the view
  // re-runs to subscribe.
  if (args.existingTerminalId != null) return false;
  if (args.awaitingAutoLaunch === true) return true;
  if (args.autoLaunchError != null) return true;
  return false;
}
