// Wire shape mirroring src-tauri/src/workspaces.rs.
// Hand-written until Rust-to-TS schema codegen lands.

import { invoke } from "@tauri-apps/api/core";

export interface SshLocation {
  user: string | null;
  host: string;
  port: number | null;
  /// Remote cwd. Required because `docs/specs/transport.md`
  /// §6.1/§6.2 include it in the remote duplicate-identity tuple —
  /// the wire shape MUST always carry a remote cwd.
  canonicalRemotePath: string;
}

export interface ContainerLocation {
  containerId: string;
  cwdInContainer: string | null;
}

export type WorkspaceLocation =
  | { kind: "local"; path: string }
  | {
      kind: "remote";
      ssh: SshLocation;
      container: ContainerLocation | null;
    };

export interface WorkspaceProfile {
  autoLaunchClaude: boolean;
  claudeArgv: string[];
  stashed?: boolean;
}

export interface WorkspaceRecord {
  workspaceId: string;
  name: string;
  location: WorkspaceLocation;
  profile: WorkspaceProfile;
  createdAt: string;
  lastUsedAt: string;
  openTabId: string | null;
  /// Best-effort claude conversation rounds count derived from
  /// scanning `<path>/.claude/` for `*.jsonl` files at command
  /// return time. NOT persisted in `workspaces.json` — purely
  /// computed for display. Always present on the wire (defaults
  /// to 0 when `.claude/` is missing or the workspace is Remote).
  conversationRoundsCount: number;
}

/// Returns the local filesystem path for `Local` workspaces, or
/// `null` for `Remote` workspaces. Frontend callers that previously
/// read `workspace.path` migrate to this helper.
export function localPath(workspace: WorkspaceRecord): string | null {
  return workspace.location.kind === "local"
    ? workspace.location.path
    : null;
}

export type WorkspaceErrorDto =
  | { kind: "invalidName"; reason: string }
  | { kind: "workspaceAlreadyExists"; path: string }
  | {
      kind: "canonicalDuplicate";
      existingWorkspaceId: string;
      existingName: string;
    }
  | { kind: "notADirectory"; path: string }
  | { kind: "notFound"; workspaceId: string }
  | { kind: "alreadyOpen"; existingTabId: string }
  | { kind: "io"; context: string; message: string }
  | { kind: "remoteFieldInvalid"; field: string; reason: string }
  | { kind: "remoteProbeFailed"; phase: string; reason: string };

export function isWorkspaceErrorDto(value: unknown): value is WorkspaceErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const k = (value as { kind?: unknown }).kind;
  return (
    k === "invalidName" ||
    k === "workspaceAlreadyExists" ||
    k === "canonicalDuplicate" ||
    k === "notADirectory" ||
    k === "notFound" ||
    k === "alreadyOpen" ||
    k === "io" ||
    k === "remoteFieldInvalid" ||
    k === "remoteProbeFailed"
  );
}

export async function listWorkspaces(): Promise<WorkspaceRecord[]> {
  return await invoke<WorkspaceRecord[]>("list_workspaces");
}

export async function createWorkspace(
  name: string,
  autoLaunchClaude: boolean,
): Promise<WorkspaceRecord> {
  return await invoke<WorkspaceRecord>("create_workspace", {
    name,
    autoLaunchClaude,
  });
}

export async function registerWorkspace(
  path: string,
  autoLaunchClaude: boolean,
): Promise<WorkspaceRecord> {
  return await invoke<WorkspaceRecord>("register_workspace", {
    path,
    autoLaunchClaude,
  });
}

/// Inputs for `registerRemoteWorkspace`. Mirrors the backend Tauri
/// command shape. Optional SSH fields (`user`, `port`) and the
/// container subform (`containerId`, `cwdInContainer`) are passed
/// as `null` when omitted so the wire layer can distinguish
/// "absent" from "empty string". Per-field validation lives on the
/// host side and surfaces as `WorkspaceErrorDto::remoteFieldInvalid`.
export interface RemoteWorkspaceFields {
  name: string;
  host: string;
  user: string | null;
  port: number | null;
  canonicalRemotePath: string;
  containerId: string | null;
  cwdInContainer: string | null;
  autoLaunchClaude: boolean;
}

export async function registerRemoteWorkspace(
  fields: RemoteWorkspaceFields,
): Promise<WorkspaceRecord> {
  return await invoke<WorkspaceRecord>("register_remote_workspace", {
    name: fields.name,
    host: fields.host,
    user: fields.user,
    port: fields.port,
    canonicalRemotePath: fields.canonicalRemotePath,
    containerId: fields.containerId,
    cwdInContainer: fields.cwdInContainer,
    autoLaunchClaude: fields.autoLaunchClaude,
  });
}

export async function openWorkspace(
  workspaceId: string,
  tabId: string,
): Promise<WorkspaceRecord> {
  return await invoke<WorkspaceRecord>("open_workspace", {
    workspaceId,
    tabId,
  });
}

export async function closeWorkspace(workspaceId: string): Promise<void> {
  await invoke<void>("close_workspace", { workspaceId });
}

export async function resolveWorkspaceForTab(
  tabId: string,
): Promise<WorkspaceRecord | null> {
  return await invoke<WorkspaceRecord | null>("resolve_workspace_for_tab", {
    tabId,
  });
}

/// Auto-launch error DTO mirroring
/// `src-tauri/src/workspace_launch_scheduler.rs::AutoLaunchErrorDto`.
export type AutoLaunchErrorDto =
  | { kind: "autoLaunchDisabled"; workspaceId: string }
  | { kind: "remoteWorkspaceNotEligible"; workspaceId: string }
  | { kind: "workspaceNotFound"; workspaceId: string }
  | { kind: "claudeDiscoveryNotReady"; discoveryKind: string; message: string }
  /// Frontend-synthesized variant (not produced by the
  /// backend invoke). When a Remote auto-launch fails
  /// asynchronously (e.g. SSH unreachable after the user closed
  /// the dialog), the backend emits a lifecycle envelope with
  /// `status=Done` + `done_reason` but no real terminal id. The
  /// frontend detects the stuck-waiting condition and
  /// synthesizes this DTO so `TerminalMeshView` can exit the
  /// waiting branch and render the error banner.
  | { kind: "asyncSpawnFailed"; transportKind: string; doneReason: string };

export function isAutoLaunchErrorDto(value: unknown): value is AutoLaunchErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const k = (value as { kind?: unknown }).kind;
  return (
    k === "autoLaunchDisabled" ||
    k === "remoteWorkspaceNotEligible" ||
    k === "workspaceNotFound" ||
    k === "claudeDiscoveryNotReady" ||
    k === "asyncSpawnFailed"
  );
}

/// Enqueue the workspace into the host-side
/// `WorkspaceLaunchScheduler`. The scheduler enforces the
/// concurrency cap (default 4) and FIFO ordering across all
/// auto-launch requests; this call returns as soon as the entry is
/// queued, NOT when the spawn finishes. The eventual terminal_id is
/// surfaced via the lifecycle snapshot subscription.
export async function requestWorkspaceAutoLaunch(
  workspaceId: string,
  tabId: string,
): Promise<void> {
  await invoke<void>("request_workspace_auto_launch", {
    workspaceId,
    tabId,
  });
}

export async function stashWorkspace(workspaceId: string): Promise<void> {
  await invoke<void>("stash_workspace", { workspaceId });
}

export async function unstashWorkspace(workspaceId: string): Promise<void> {
  await invoke<void>("unstash_workspace", { workspaceId });
}
