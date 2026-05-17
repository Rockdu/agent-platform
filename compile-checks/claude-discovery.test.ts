// Compile-time probe for the claude-discovery wire shape (Round 20 /
// task13 / AC-2.2 + AC-2.3 + AC-3.2). Mirrors
// src-tauri/src/claude_discovery.rs::{ClaudePathRecord,
// ClaudeDiscoveryStatus, ClaudeDiscoveryErrorDto}. If either side
// drifts, `tsc -b` here is the first failure surface.

import type {
  ClaudeDiscoveryErrorDto,
  ClaudeDiscoveryStatus,
  ClaudePathRecord,
} from "../src/claude-discovery";

export function _probe_record_shape(r: ClaudePathRecord): string {
  return `${r.path}|${r.version ?? "<none>"}|${r.discoveredAt}`;
}

export function _probe_status_discrimination(s: ClaudeDiscoveryStatus): string {
  switch (s.kind) {
    case "ready":
      return `ready:${s.record.path}:${s.record.version ?? "<none>"}`;
    case "not_found":
      return `not_found:${s.probed.length}`;
    case "not_run":
      return "not_run";
  }
}

export function _probe_error_dto_discrimination(e: ClaudeDiscoveryErrorDto): string {
  switch (e.kind) {
    case "claudeNotFound":
      return `claudeNotFound:${e.probed.length}`;
    case "notExecutable":
      return `notExecutable:${e.path}`;
    case "versionProbeFailed":
      return `versionProbeFailed:${e.path}:${e.message}`;
    case "io":
      return `io:${e.context}:${e.message}`;
  }
}

// Object-literal probes: tsc reports the type-mismatch on the offending
// field, so `@ts-expect-error` sits directly above each row.
export function _probe_invalid_status_ready_missing_path(): void {
  const s: ClaudeDiscoveryStatus = {
    kind: "ready",
    // @ts-expect-error `record.path` is required
    record: {
      version: "2.0.0",
      discoveredAt: "2026-05-16T00:00:00Z",
    },
  };
  void s;
}

export function _probe_invalid_error_dto_kind(): void {
  const e: ClaudeDiscoveryErrorDto = {
    // @ts-expect-error kind must be one of the named ClaudeDiscoveryErrorDto variants
    kind: "not-a-real-kind",
    message: "bogus",
  };
  void e;
}
