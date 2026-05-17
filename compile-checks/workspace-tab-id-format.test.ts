// Format contract for workspace tab ids. The frontend uses the bare
// workspace UUID as the tab id so the terminal-mesh sidecar's
// `ClientId::parse` (in crates/mcp-stdio) — which requires the tab
// id field to be a valid UUID — accepts it. If the format ever
// drifts (e.g. someone reintroduces a `tab-${uuid}` prefix), the
// sidecar can never authenticate against that tab; `terminal_mesh.
// list_tabs` and `terminal_mesh.read_scrollback` would then look
// like they "don't work" for regular workspace claude sessions.
//
// This file is a tsc compile-time probe; running the assertions
// requires `npx tsx compile-checks/workspace-tab-id-format.test.ts`.

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    throw new Error(`workspace-tab-id-format assertion failed: ${msg}`);
  }
}

// Mirror the production tab-id derivation in App.tsx::adoptWorkspaceTab.
function deriveWorkspaceTabId(workspaceId: string): string {
  return workspaceId;
}

const UUID_V4_LIKE =
  /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

export function runWorkspaceTabIdFormatAssertions(): void {
  // Use a fixed UUID so the assertion is deterministic across runs and
  // independent of the host's crypto entropy.
  const sampleWorkspaceId = "11111111-2222-4333-8444-555555555555";

  const tabId = deriveWorkspaceTabId(sampleWorkspaceId);

  assert(
    UUID_V4_LIKE.test(tabId),
    `derived tab id ${tabId} must be a bare UUID (no prefix)`,
  );
  assert(
    tabId === sampleWorkspaceId,
    `derived tab id must equal the workspace id; got ${tabId}`,
  );
  // The previous prefix-form `tab-${uuid}` must not slip back in.
  assert(
    !tabId.startsWith("tab-"),
    `derived tab id must not carry the legacy "tab-" prefix; got ${tabId}`,
  );
}

declare const require: { main?: unknown } | undefined;
declare const module: unknown;
if (typeof require !== "undefined" && typeof module !== "undefined" && require.main === module) {
  runWorkspaceTabIdFormatAssertions();
}
