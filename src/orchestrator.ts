// Wire shape mirroring src-tauri/src/orchestrator.rs.
// Hand-written until Rust-to-TS schema codegen lands.

import { invoke } from "@tauri-apps/api/core";

export interface OrchestratorSession {
  terminalId: string;
  tabId: string;
  mcpConfigPath: string;
}

export type OrchestratorStatus =
  | { kind: "ready"; session: OrchestratorSession }
  | { kind: "notLaunched" }
  | { kind: "claudeMissing" };

export type OrchestratorErrorDto =
  | { kind: "claudeMissing" }
  | { kind: "mcpConfigFailed"; message: string }
  | { kind: "spawnFailed"; message: string }
  | { kind: "io"; context: string; message: string };

export function isOrchestratorErrorDto(
  value: unknown,
): value is OrchestratorErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const k = (value as { kind?: unknown }).kind;
  return (
    k === "claudeMissing" ||
    k === "mcpConfigFailed" ||
    k === "spawnFailed" ||
    k === "io"
  );
}

export async function getOrchestratorStatus(): Promise<OrchestratorStatus> {
  return await invoke<OrchestratorStatus>("orchestrator_status");
}

export async function launchOrchestratorClaude(): Promise<OrchestratorStatus> {
  return await invoke<OrchestratorStatus>("orchestrator_launch_claude");
}

export async function shutdownOrchestrator(): Promise<void> {
  await invoke<void>("orchestrator_shutdown");
}
