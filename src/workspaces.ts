// Wire shape mirroring src-tauri/src/workspaces.rs.
// Hand-written until Rust-to-TS schema codegen lands.

import { invoke } from "@tauri-apps/api/core";

export interface WorkspaceRecord {
  workspaceId: string;
  name: string;
  path: string;
  createdAt: string;
  lastUsedAt: string;
  openTabId: string | null;
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
  | { kind: "io"; context: string; message: string };

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
    k === "io"
  );
}

export async function listWorkspaces(): Promise<WorkspaceRecord[]> {
  return await invoke<WorkspaceRecord[]>("list_workspaces");
}

export async function createWorkspace(name: string): Promise<WorkspaceRecord> {
  return await invoke<WorkspaceRecord>("create_workspace", { name });
}

export async function registerWorkspace(path: string): Promise<WorkspaceRecord> {
  return await invoke<WorkspaceRecord>("register_workspace", { path });
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
