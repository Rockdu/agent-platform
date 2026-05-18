// Pure-helper compile-check for the `.claude` rounds cell. Local
// workspaces render their conversation count (or `"无"` when
// zero). Remote workspaces render an unavailability label and
// carry an explanatory tooltip; the `.claude/` directory lives on
// the remote machine and the host cannot scan it.
//
// Runs under tsx; no DOM / no React imports. Exits non-zero on
// any assertion failure so the rlcr verifier can pin the contract
// at the wire-shape + helper-output level.

import {
  AUTO_LAUNCH_BOUNDARY_TOOLTIP,
  DEFAULT_AUTO_LAUNCH_CLAUDE,
  formatClaudeRoundsCell,
  claudeRoundsCellTooltip,
} from "../src/WorkspaceSwitcherModal";
import type { WorkspaceRecord } from "../src/workspaces";

function assertEq<T>(label: string, actual: T, expected: T): void {
  if (actual !== expected) {
    console.error(
      `FAIL ${label}: expected ${JSON.stringify(expected)} but got ${JSON.stringify(actual)}`,
    );
    process.exit(1);
  }
  console.log(`PASS ${label}`);
}

const local_with_rounds: WorkspaceRecord = {
  workspaceId: "00000000-0000-0000-0000-000000000001",
  name: "local-a",
  location: { kind: "local", path: "/tmp/a" },
  profile: { autoLaunchClaude: true, claudeArgv: ["--dangerously-skip-permissions"] },
  createdAt: "2026-01-01T00:00:00Z",
  lastUsedAt: "2026-01-01T00:00:00Z",
  openTabId: null,
  conversationRoundsCount: 3,
};
const local_zero_rounds: WorkspaceRecord = {
  ...local_with_rounds,
  workspaceId: "00000000-0000-0000-0000-000000000002",
  conversationRoundsCount: 0,
};
const remote_ssh: WorkspaceRecord = {
  workspaceId: "00000000-0000-0000-0000-000000000003",
  name: "remote-ssh",
  location: {
    kind: "remote",
    ssh: {
      user: "alice",
      host: "h.example",
      port: 22,
      canonicalRemotePath: "/srv",
    },
    container: null,
  },
  profile: { autoLaunchClaude: true, claudeArgv: ["--dangerously-skip-permissions"] },
  createdAt: "2026-01-01T00:00:00Z",
  lastUsedAt: "2026-01-01T00:00:00Z",
  openTabId: null,
  conversationRoundsCount: 0,
};
const remote_docker: WorkspaceRecord = {
  ...remote_ssh,
  workspaceId: "00000000-0000-0000-0000-000000000004",
  location: {
    kind: "remote",
    ssh: {
      user: null,
      host: "h.example",
      port: null,
      canonicalRemotePath: "/srv",
    },
    container: { containerId: "abc", cwdInContainer: "/work" },
  },
};

assertEq(
  "local with rounds renders count",
  formatClaudeRoundsCell(local_with_rounds),
  "claude 对话: 3 轮",
);
assertEq(
  "local zero rounds renders 无",
  formatClaudeRoundsCell(local_zero_rounds),
  "claude 对话: 无",
);
assertEq(
  "remote SSH renders 远程不可用",
  formatClaudeRoundsCell(remote_ssh),
  "claude 对话: 远程不可用",
);
assertEq(
  "remote Docker renders 远程不可用",
  formatClaudeRoundsCell(remote_docker),
  "claude 对话: 远程不可用",
);
assertEq(
  "local tooltip is undefined",
  claudeRoundsCellTooltip(local_with_rounds),
  undefined,
);
assertEq(
  "remote tooltip explains unavailability",
  claudeRoundsCellTooltip(remote_ssh),
  "远程工作区的 .claude/ 在远端机器，本机无法扫描",
);

// Auto-launch checkbox tooltip MUST mention both the
// permission-skipping behavior AND the confirm-on-write boundary
// so users see the tradeoff at decision time.
function assertContains(label: string, haystack: string, needle: string): void {
  if (!haystack.includes(needle)) {
    console.error(
      `FAIL ${label}: tooltip is missing required phrase ${JSON.stringify(needle)}\n  tooltip: ${haystack}`,
    );
    process.exit(1);
  }
  console.log(`PASS ${label}`);
}
assertContains(
  "auto-launch tooltip mentions --dangerously-skip-permissions",
  AUTO_LAUNCH_BOUNDARY_TOOLTIP,
  "--dangerously-skip-permissions",
);
assertContains(
  "auto-launch tooltip mentions confirm-on-write boundary",
  AUTO_LAUNCH_BOUNDARY_TOOLTIP,
  "confirm-on-write",
);
assertContains(
  "auto-launch tooltip mentions plugin-mediated writes",
  AUTO_LAUNCH_BOUNDARY_TOOLTIP,
  "插件",
);

// Default auto-launch checkbox state MUST be CHECKED per the
// product spec. The modal's reset path resets to this constant
// on close, so unchecking once does not silently persist across
// reopens.
assertEq(
  "auto-launch default is checked",
  DEFAULT_AUTO_LAUNCH_CLAUDE,
  true,
);

console.log("workspace-claude-rounds-cell: all assertions passed");
