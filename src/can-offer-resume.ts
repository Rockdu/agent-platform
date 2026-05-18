// Pure helper: decide whether a Done row's resume affordance
// ("下一条指令") should be clickable.
//
// The Done section of the workspace rail offers a click target
// that focuses the tab so the next keystroke flows through
// `terminal_write_stdin` and transitions the snapshot back to
// Running. But this only works when the backend's actor + PTY
// are still ALIVE. The `Done` status covers four `DoneReason`
// kinds with two distinct lifecycle outcomes:
//
//  - `TaskComplete`: the foreground claude signaled task done
//    via OSC; the actor + PTY + child shell are still alive.
//    Stdin write succeeds, transitions back to Running. ✓
//
//  - `CleanCompletion` / `NonZeroExit` / `Disconnected`: the
//    actor has exited (clean / non-zero / lost-connection); the
//    backend has dropped the live command channel. Stdin write
//    hits `NotFound` on the backend → no recovery. ✗
//
// The affordance must be hidden for the dead-actor cases so the
// user doesn't click a dead button.

import type { WorkspaceLifecycleSnapshot } from "./terminal-mesh";

export function canOfferResumeFromDone(
  snapshot: WorkspaceLifecycleSnapshot,
): boolean {
  if (snapshot.status !== "Done") return false;
  const reason = snapshot.doneReason;
  if (reason === null) return false;
  return reason.kind === "TaskComplete";
}
