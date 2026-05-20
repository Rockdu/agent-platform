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

/// Open a remote SSH workspace in Cursor / VS Code via the
/// `vscode-remote://ssh-remote+[user@]host[:port]/path` URI scheme.
/// Open a Docker container workspace in Cursor/VS Code using the Dev Containers
/// extension. For remote containers, sets DOCKER_HOST=ssh://host so the local
/// Docker client tunnels to the remote daemon — then uses the attached-container
/// URI scheme for direct one-step container attachment.
export async function openDockerWorkspaceInIde(params: {
  sshUser: string | null;
  sshHost: string | null;
  sshPort: number | null;
  containerId: string;
  cwdInContainer: string;
}): Promise<void> {
  await invoke<void>("ide_open_docker_workspace", {
    sshUser: params.sshUser,
    sshHost: params.sshHost,
    sshPort: params.sshPort,
    containerId: params.containerId,
    cwdInContainer: params.cwdInContainer,
  });
}

export async function openRemoteWorkspaceInIde(params: {
  sshUser: string | null;
  sshHost: string;
  sshPort: number | null;
  remotePath: string;
}): Promise<void> {
  await invoke<void>("ide_open_remote_workspace", {
    sshUser: params.sshUser,
    sshHost: params.sshHost,
    sshPort: params.sshPort,
    remotePath: params.remotePath,
  });
}

export async function revealWorkspaceInFinder(workspacePath: string): Promise<void> {
  await invoke<void>("ide_reveal_in_finder", { workspacePath });
}
