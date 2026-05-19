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
  /**
   * Persisted workspace id this terminal is bound to. The host
   * stores it in the lifecycle snapshot so the
   * `terminal_mesh.list_tabs` MCP tool can project real workspace ids
   * to MCP clients. Orchestrator-routed and transient terminals omit
   * this field.
   */
  workspaceId?: string;
}

export interface TerminalSpawnResponse {
  terminalId: string;
}

export type AttentionSeverity = "Info" | "NeedsConfirm" | "Error";

export type AttentionKind =
  | { kind: "completion"; exitCode: number }
  | { kind: "nonZeroExit"; exitCode: number }
  | { kind: "promptWaiting" }
  | { kind: "agentMarker"; summary: string | null; severity: AttentionSeverity }
  | { kind: "disconnect" }
  | { kind: "taskComplete"; summary: string };

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

// Host-side lifecycle snapshot consumed by the inner-rail Running/
// Done queue, the upcoming `terminal_mesh.list_tabs` MCP tool, and
// the orchestrator-isolation filter. Wire shape mirrors
// `src-tauri/src/workspace_lifecycle.rs`.
export type TabKind = "Orchestrator" | "Workspace";
export type TransportKind = "Local" | "Ssh" | "SshDocker";
export type TabStatus = "Running" | "Done";

export type DoneReason =
  | { kind: "CleanCompletion" }
  | { kind: "NonZeroExit"; code: number }
  | { kind: "Disconnected" }
  | { kind: "TaskComplete"; summary: string };

export interface WorkspaceLifecycleSnapshot {
  workspaceId: string | null;
  tabKind: TabKind;
  transportKind: TransportKind;
  status: TabStatus;
  doneReason: DoneReason | null;
  lastActivityAtUnixMs: number;
  /// `true` while the workspace is waiting in the host-side launch
  /// scheduler queue (concurrency cap is saturated). The rail row
  /// renders a `等待启动` badge instead of the default Running badge
  /// when this is true.
  pendingLaunch: boolean;
  /// `true` while the agent is actively working on a user-submitted
  /// task. Set to true on user-initiated stdin; reset to false when a
  /// Done transition fires (TaskComplete, exit, disconnect). Drives
  /// the 完成区/运行区 split: Running+agentBusy=false → 完成区 (idle,
  /// waiting for instruction); Running+agentBusy=true → 运行区 (busy).
  agentBusy: boolean;
}

// Single envelope shape shared between the initial `workspace_
// lifecycle_snapshot` fetch and the `lifecycle://updated` event
// stream so the frontend hook can apply the same handler to both.
//
// `terminalId` is `null` while a workspace is sitting in the
// launch-scheduler queue (pre-spawn placeholder). The hook treats
// `null` as "snapshot data only; do NOT mark this tab as resolved
// in `terminalIdByTabId`" — so the later real-terminal event still
// triggers the resolved-id update instead of being suppressed by
// the "already known" gate.
//
// `tabId` is always set for placeholder events (the only routing
// key available) and helpful even for real events so the hook can
// prune stale refs without a reverse lookup.
export interface LifecycleUpdateEvent {
  terminalId: string | null;
  tabId: string | null;
  snapshot: WorkspaceLifecycleSnapshot;
}

export const LIFECYCLE_UPDATED_TOPIC = "lifecycle://updated";

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
  userInitiated?: boolean,
): Promise<void> {
  await invoke<void>("terminal_write_stdin", {
    terminalId,
    data,
    userInitiated,
  });
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

// ----- Lifecycle (Running/Done queue) wire functions -----

export async function workspaceLifecycleSnapshot(
  tabId: string,
): Promise<LifecycleUpdateEvent | null> {
  const entry = await invoke<LifecycleUpdateEvent | null>(
    "workspace_lifecycle_snapshot",
    { tabId },
  );
  return entry ?? null;
}

export async function subscribeWorkspaceLifecycleUpdates(
  onUpdate: (event: LifecycleUpdateEvent) => void,
): Promise<UnlistenFn> {
  return await listen<LifecycleUpdateEvent>(
    LIFECYCLE_UPDATED_TOPIC,
    (msg) => {
      onUpdate(msg.payload);
    },
  );
}
