// Pure-helper compile-check for the stuck-auto-launch detection
// that drives the "auto-launch failed asynchronously → reveal the
// banner" transition in App.tsx.
//
// The condition is: the tab is awaiting auto-launch, its retained
// snapshot has reached Done, AND no real terminal id has resolved
// through the lifecycle hook. The helper returns the
// AutoLaunchErrorDto to install, or null when no transition
// should fire.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure.

import { synthesizeStuckAutoLaunchError } from "../src/stuck-auto-launch";
import type { WorkspaceLifecycleSnapshot } from "../src/terminal-mesh";

function mkSnapshot(
  status: "Running" | "Done",
  doneReason: WorkspaceLifecycleSnapshot["doneReason"],
  transportKind: WorkspaceLifecycleSnapshot["transportKind"] = "Ssh",
): WorkspaceLifecycleSnapshot {
  return {
    workspaceId: null,
    tabKind: "Workspace",
    transportKind,
    status,
    doneReason,
    lastActivityAtUnixMs: 0,
    pendingLaunch: false,
    agentBusy: false,
  };
}

function fail(label: string, why: string): never {
  console.error(`FAIL ${label}: ${why}`);
  process.exit(1);
}

// 1. Stuck-waiting condition: awaiting + Done + no real id →
// returns the synthesized error with the right transport + reason.
{
  const r = synthesizeStuckAutoLaunchError(
    { tabId: "tab-a", awaitingAutoLaunch: true },
    mkSnapshot("Done", { kind: "Disconnected" }, "Ssh"),
    undefined,
  );
  if (r === null) fail("ssh disconnect stuck → returns error", "got null");
  if (r.kind !== "asyncSpawnFailed")
    fail("ssh disconnect stuck → kind", `got ${r.kind}`);
  if (r.transportKind !== "Ssh")
    fail("ssh disconnect stuck → transportKind", r.transportKind);
  if (r.doneReason !== "Disconnected")
    fail("ssh disconnect stuck → doneReason", r.doneReason);
  console.log("PASS ssh disconnect stuck → returns error");
}

// 2. Awaiting + Running → no transition; we still expect the
// terminal id to arrive shortly.
{
  const r = synthesizeStuckAutoLaunchError(
    { tabId: "tab-a", awaitingAutoLaunch: true },
    mkSnapshot("Running", null),
    undefined,
  );
  if (r !== null) fail("awaiting + Running → null", `got ${JSON.stringify(r)}`);
  console.log("PASS awaiting + Running → null");
}

// 3. Awaiting + Done + real id present → no transition; the id
// will resolve via the normal attach path, and the view should
// not show the failure banner because the launch succeeded.
{
  const r = synthesizeStuckAutoLaunchError(
    { tabId: "tab-a", awaitingAutoLaunch: true },
    mkSnapshot("Done", { kind: "CleanCompletion" }, "SshDocker"),
    "term-real-1",
  );
  if (r !== null)
    fail("awaiting + Done + real id → null", `got ${JSON.stringify(r)}`);
  console.log("PASS awaiting + Done + real id → null");
}

// 4. Not awaiting → null regardless of snapshot.
{
  const r = synthesizeStuckAutoLaunchError(
    { tabId: "tab-a", awaitingAutoLaunch: false },
    mkSnapshot("Done", { kind: "Disconnected" }, "Ssh"),
    undefined,
  );
  if (r !== null) fail("not awaiting → null", `got ${JSON.stringify(r)}`);
  console.log("PASS not awaiting → null");
}

// 5. No snapshot yet → null (the hook has not fetched yet).
{
  const r = synthesizeStuckAutoLaunchError(
    { tabId: "tab-a", awaitingAutoLaunch: true },
    undefined,
    undefined,
  );
  if (r !== null) fail("no snapshot → null", `got ${JSON.stringify(r)}`);
  console.log("PASS no snapshot → null");
}

// 6. NonZeroExit reason flows through with the exit code.
{
  const r = synthesizeStuckAutoLaunchError(
    { tabId: "tab-a", awaitingAutoLaunch: true },
    mkSnapshot("Done", { kind: "NonZeroExit", code: 137 }, "Local"),
    undefined,
  );
  if (r === null) fail("NonZeroExit stuck → returns error", "got null");
  if (r.kind !== "asyncSpawnFailed") fail("NonZeroExit stuck → kind", r.kind);
  if (r.doneReason !== "NonZeroExit(137)")
    fail("NonZeroExit stuck → doneReason", r.doneReason);
  console.log("PASS NonZeroExit stuck → returns error with code");
}

console.log("stuck-auto-launch: all assertions passed");
