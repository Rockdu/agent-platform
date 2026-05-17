// Wire shape mirroring src-tauri/src/ide_handoff.rs.
// Hand-written until Rust-to-TS schema codegen lands.

import { invoke } from "@tauri-apps/api/core";

export interface IdePreference {
  ideCommand: string;
  ideArgsTemplate: string[];
}

export type IdeHandoffErrorDto =
  | { kind: "ideNotInPath"; command: string }
  | { kind: "notADirectory"; path: string }
  | { kind: "spawnFailed"; command: string; message: string }
  | { kind: "io"; context: string; message: string };

export function isIdeHandoffErrorDto(value: unknown): value is IdeHandoffErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const k = (value as { kind?: unknown }).kind;
  return (
    k === "ideNotInPath" ||
    k === "notADirectory" ||
    k === "spawnFailed" ||
    k === "io"
  );
}

export async function getIdePreference(): Promise<IdePreference> {
  return await invoke<IdePreference>("ide_get_preference");
}

export async function setIdePreference(
  preference: IdePreference,
): Promise<IdePreference> {
  return await invoke<IdePreference>("ide_set_preference", { preference });
}

export async function openWorkspaceInIde(workspacePath: string): Promise<void> {
  await invoke<void>("ide_open_workspace", { workspacePath });
}

export async function revealWorkspaceInFinder(workspacePath: string): Promise<void> {
  await invoke<void>("ide_reveal_in_finder", { workspacePath });
}
