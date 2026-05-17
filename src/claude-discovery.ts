// Wire shape mirroring src-tauri/src/claude_discovery.rs::{ClaudePathRecord,
// ClaudeDiscoveryStatus, ClaudeDiscoveryErrorDto}. Hand-written until
// Rust-to-TS schema codegen lands.

import { invoke } from "@tauri-apps/api/core";

export interface ClaudePathRecord {
  path: string;
  version: string | null;
  discoveredAt: string;
}

export type ClaudeDiscoveryStatus =
  | { kind: "ready"; record: ClaudePathRecord }
  | { kind: "not_found"; probed: string[] }
  | { kind: "not_run" };

export type ClaudeDiscoveryErrorDto =
  | { kind: "claudeNotFound"; probed: string[] }
  | { kind: "notExecutable"; path: string }
  | { kind: "versionProbeFailed"; path: string; message: string }
  | { kind: "io"; context: string; message: string };

export async function getClaudeDiscoveryStatus(): Promise<ClaudeDiscoveryStatus> {
  return await invoke<ClaudeDiscoveryStatus>("claude_discovery_status");
}

export async function redoClaudeDiscovery(): Promise<ClaudePathRecord> {
  return await invoke<ClaudePathRecord>("claude_redo_discovery");
}

export async function setClaudePathOverride(
  path: string,
): Promise<ClaudePathRecord> {
  return await invoke<ClaudePathRecord>("claude_set_path_override", { path });
}

export function isClaudeDiscoveryErrorDto(
  value: unknown,
): value is ClaudeDiscoveryErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const kind = (value as { kind?: unknown }).kind;
  return (
    kind === "claudeNotFound" ||
    kind === "notExecutable" ||
    kind === "versionProbeFailed" ||
    kind === "io"
  );
}
