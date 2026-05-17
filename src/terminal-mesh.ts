// Wire shape mirroring src-tauri/src/terminal_mesh.rs.
// Hand-written until Rust-to-TS schema codegen lands.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface TerminalSpawnRequest {
  cwd?: string;
  env?: Array<[string, string]>;
  cols?: number;
  rows?: number;
  /**
   * task21 / AC-3.3: workspace tab id this terminal belongs to. Threaded
   * into `TerminalMeshRegistry.tab_index` so the host RPC bridge can
   * resolve `target_tab_id → terminal_id` for cross-tab read requests
   * coming from the orchestrator's claude (and for regular tabs to
   * self-read their own scrollback through the same bridge surface).
   */
  tabId?: string;
}

export interface TerminalSpawnResponse {
  terminalId: string;
}

export type AttentionSeverity = "Info" | "NeedsConfirm" | "Error";

export type AttentionKind =
  | { kind: "completion"; exitCode: number }
  | { kind: "nonZeroExit"; exitCode: number }
  | { kind: "promptWaiting" }
  | { kind: "agentMarker"; summary: string | null; severity: AttentionSeverity };

export interface NeedsAttentionPayload {
  eventId: string;
  dedupKey: string;
  kind: AttentionKind;
}

export type TerminalEvent =
  | { kind: "output"; bytes: number[] }
  | { kind: "resize"; cols: number; rows: number }
  | { kind: "exit"; code: number | null }
  | { kind: "needsAttention"; payload: NeedsAttentionPayload }
  | { kind: "cancelled" };

export interface TerminalEventEnvelope {
  terminalId: string;
  pluginId: string;
  timestamp: { secs_since_epoch: number; nanos_since_epoch: number };
  event: TerminalEvent;
}

export interface BufferTruncated {
  terminalId: string;
  pluginId: string;
  timestamp: { secs_since_epoch: number; nanos_since_epoch: number };
  bytesDropped: number;
}

export type TerminalMeshErrorDto =
  | { kind: "notFound"; terminalId: string }
  | { kind: "invalidTerminalId"; raw: string; message: string }
  | { kind: "spawn"; message: string }
  | { kind: "io"; context: string; message: string }
  | { kind: "permissionDenied"; message: string };

export function isTerminalMeshErrorDto(
  value: unknown,
): value is TerminalMeshErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const k = (value as { kind?: unknown }).kind;
  return (
    k === "notFound" ||
    k === "invalidTerminalId" ||
    k === "spawn" ||
    k === "io" ||
    k === "permissionDenied"
  );
}

export async function spawnTerminal(
  req: TerminalSpawnRequest,
): Promise<TerminalSpawnResponse> {
  return await invoke<TerminalSpawnResponse>("terminal_spawn", { req });
}

export async function writeTerminalStdin(
  terminalId: string,
  data: string,
): Promise<void> {
  await invoke<void>("terminal_write_stdin", { terminalId, data });
}

export async function resizeTerminal(
  terminalId: string,
  cols: number,
  rows: number,
): Promise<void> {
  await invoke<void>("terminal_resize", { terminalId, cols, rows });
}

export async function shutdownTerminal(terminalId: string): Promise<void> {
  await invoke<void>("terminal_shutdown", { terminalId });
}

export async function readTerminalScrollback(
  terminalId: string,
  maxBytes: number,
): Promise<string> {
  return await invoke<string>("terminal_scrollback", {
    terminalId,
    maxBytes,
  });
}

export function eventTopic(terminalId: string): string {
  return `terminal://${terminalId}/event`;
}

export function statusTopic(terminalId: string): string {
  return `terminal://${terminalId}/status`;
}

export async function subscribeTerminalEvents(
  terminalId: string,
  onEvent: (envelope: TerminalEventEnvelope) => void,
): Promise<UnlistenFn> {
  return await listen<TerminalEventEnvelope>(eventTopic(terminalId), (msg) => {
    onEvent(msg.payload);
  });
}

export async function subscribeTerminalStatus(
  terminalId: string,
  onStatus: (status: BufferTruncated) => void,
): Promise<UnlistenFn> {
  return await listen<BufferTruncated>(statusTopic(terminalId), (msg) => {
    onStatus(msg.payload);
  });
}
