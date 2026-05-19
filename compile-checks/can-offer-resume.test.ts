// Pure-helper compile-check for the Done-row resume affordance
// gating. The Done section's click target only works for
// `TaskComplete` (actor + PTY still alive). The other Done
// reasons correspond to actor exit; stdin write fails.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { canOfferResumeFromDone } from "../src/can-offer-resume";
import type {
  DoneReason,
  WorkspaceLifecycleSnapshot,
} from "../src/terminal-mesh";

function fail(label: string, why: string): never {
  console.error(`FAIL ${label}: ${why}`);
  process.exit(1);
}

function makeSnap(overrides: Partial<WorkspaceLifecycleSnapshot> = {}): WorkspaceLifecycleSnapshot {
  return {
    workspaceId: null,
    tabKind: "Workspace",
    transportKind: "Local",
    status: "Running",
    doneReason: null,
    lastActivityAtUnixMs: 0,
    pendingLaunch: false,
    promptVisible: false,
    ...overrides,
  };
}

// 1. Done + TaskComplete → resume offered (actor still alive).
{
  const reason: DoneReason = { kind: "TaskComplete", summary: "all green" };
  const snap = makeSnap({ status: "Done", doneReason: reason });
  if (!canOfferResumeFromDone(snap))
    fail("TaskComplete → offer", "TaskComplete keeps the actor alive");
  console.log("PASS TaskComplete Done offers resume");
}

// 2. Done + CleanCompletion → NO resume (actor exited).
{
  const snap = makeSnap({
    status: "Done",
    doneReason: { kind: "CleanCompletion" },
  });
  if (canOfferResumeFromDone(snap))
    fail("CleanCompletion → hide", "actor has exited; stdin would NotFound");
  console.log("PASS CleanCompletion Done hides resume");
}

// 3. Done + NonZeroExit → NO resume.
{
  const snap = makeSnap({
    status: "Done",
    doneReason: { kind: "NonZeroExit", code: 137 },
  });
  if (canOfferResumeFromDone(snap))
    fail("NonZeroExit → hide", "actor exited non-zero");
  console.log("PASS NonZeroExit Done hides resume");
}

// 4. Done + Disconnected → NO resume.
{
  const snap = makeSnap({
    status: "Done",
    doneReason: { kind: "Disconnected" },
  });
  if (canOfferResumeFromDone(snap))
    fail("Disconnected → hide", "transport lost; no live channel");
  console.log("PASS Disconnected Done hides resume");
}

// 5. Done + null reason (defensive default) → NO resume.
{
  const snap = makeSnap({ status: "Done", doneReason: null });
  if (canOfferResumeFromDone(snap))
    fail("Done+null reason → hide", "unknown done reason — default safe");
  console.log("PASS Done with null reason hides resume");
}

// 6. Running status (any reason) → NO resume.
// The affordance is Done-section-only by design; this is a
// safety net for the helper itself.
{
  const snap = makeSnap({
    status: "Running",
    doneReason: { kind: "TaskComplete", summary: "x" },
  });
  if (canOfferResumeFromDone(snap))
    fail("Running → hide", "affordance is Done-only by design");
  console.log("PASS Running hides resume (regardless of reason)");
}

// 7. Pending placeholder (Running + pendingLaunch=true) → NO resume.
{
  const snap = makeSnap({ status: "Running", pendingLaunch: true });
  if (canOfferResumeFromDone(snap))
    fail("Pending → hide", "pre-spawn placeholder has no tab to focus");
  console.log("PASS Pending placeholder hides resume");
}

console.log("can-offer-resume: all assertions passed");
