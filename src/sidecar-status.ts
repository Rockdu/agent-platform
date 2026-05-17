// Wire shape mirroring src-tauri/src/sidecar_manager.rs::SidecarStatusSnapshot
// and SidecarErrorDto. Hand-written until Rust-to-TS schema codegen lands.

export interface SidecarStatusSnapshot {
  clientId: string;
  pluginId: string;
  pid: number | null;
  state: string;
  generation: number;
  recentRestartCount: number;
  nextBackoffMs: number;
  shutdownRequested: boolean;
}

export type SidecarErrorDto =
  | { kind: "alreadyMounted"; clientId: string }
  | { kind: "notFound"; clientId: string }
  | { kind: "invalidState"; clientId: string; currentState: string }
  | { kind: "invalidClientId"; raw: string; message: string }
  | { kind: "unknownPlugin"; pluginId: string }
  | {
      kind: "missingBinary";
      pluginId: string;
      commandBin: string;
      candidates: string[];
    }
  | { kind: "io"; context: string; message: string }
  | { kind: "encodeShutdown"; message: string }
  | { kind: "signal"; message: string };

export type SidecarLiveState =
  | "spawning"
  | "ready"
  | "backingOff"
  | "exited"
  | "unrecoverable"
  | "shuttingDown"
  | "transportCorrupt";

export function classifySidecarState(
  state: string,
): SidecarLiveState | "unknown" {
  if (state.startsWith("Ready")) return "ready";
  if (state.startsWith("Spawning")) return "spawning";
  if (state.startsWith("BackingOff")) return "backingOff";
  if (state.startsWith("Exited")) return "exited";
  if (state.startsWith("Unrecoverable")) return "unrecoverable";
  if (state.startsWith("ShuttingDown")) return "shuttingDown";
  if (state.startsWith("TransportCorrupt")) return "transportCorrupt";
  return "unknown";
}

export function hostUiClientId(pluginId: string): string {
  return `host_ui:${pluginId}`;
}
