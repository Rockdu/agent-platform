// Compile-time probe for the sidecar-status wire shape (Round 19 / task11 v2 /
// AC-1.6 + AC-1.7). Mirrors src-tauri/src/sidecar_manager.rs::
// {SidecarStatusSnapshot, SidecarErrorDto}. If either the Rust types or the
// TS mirrors drift, `tsc -b` here is the first failure surface.

import {
  classifySidecarState,
  hostUiClientId,
  type SidecarErrorDto,
  type SidecarStatusSnapshot,
} from "../src/sidecar-status";

export function _probe_status_shape(s: SidecarStatusSnapshot): string {
  return [
    s.clientId,
    s.pluginId,
    s.pid ?? "<no-pid>",
    s.state,
    s.generation,
    s.recentRestartCount,
    s.nextBackoffMs,
    s.shutdownRequested,
  ].join("|");
}

export function _probe_error_dto_discrimination(e: SidecarErrorDto): string {
  switch (e.kind) {
    case "alreadyMounted":
      return `alreadyMounted:${e.clientId}`;
    case "notFound":
      return `notFound:${e.clientId}`;
    case "invalidState":
      return `invalidState:${e.clientId}:${e.currentState}`;
    case "invalidClientId":
      return `invalidClientId:${e.raw}:${e.message}`;
    case "unknownPlugin":
      return `unknownPlugin:${e.pluginId}`;
    case "missingBinary":
      return `missingBinary:${e.pluginId}:${e.commandBin}:${e.candidates.length}`;
    case "io":
      return `io:${e.context}:${e.message}`;
    case "encodeShutdown":
      return `encodeShutdown:${e.message}`;
    case "signal":
      return `signal:${e.message}`;
  }
}

export function _probe_classify_state(): string {
  return [
    classifySidecarState("Ready"),
    classifySidecarState("BackingOff(50ms)"),
    classifySidecarState("Unrecoverable(...)"),
    classifySidecarState("Spawning"),
    classifySidecarState("Exited(...)"),
    classifySidecarState("ShuttingDown"),
    classifySidecarState("TransportCorrupt"),
    classifySidecarState("Unknown"),
  ].join(",");
}

export function _probe_host_ui_client_id(): string {
  return hostUiClientId("example-notes");
}

// Object-literal probes — tsc places the type-mismatch error on the offending
// field, so `@ts-expect-error` directives sit immediately above each row.
export function _probe_invalid_status_pid_type(): void {
  const s: SidecarStatusSnapshot = {
    clientId: "host_ui:example-notes",
    pluginId: "example-notes",
    // @ts-expect-error pid must be number | null, not string
    pid: "1234",
    state: "Ready",
    generation: 1,
    recentRestartCount: 0,
    nextBackoffMs: 50,
    shutdownRequested: false,
  };
  void s;
}

export function _probe_invalid_status_missing_field(): void {
  // @ts-expect-error generation is required
  const s: SidecarStatusSnapshot = {
    clientId: "host_ui:example-notes",
    pluginId: "example-notes",
    pid: null,
    state: "Ready",
    recentRestartCount: 0,
    nextBackoffMs: 50,
    shutdownRequested: false,
  };
  void s;
}

export function _probe_invalid_error_dto_kind(): void {
  const e: SidecarErrorDto = {
    // @ts-expect-error kind must be one of the named SidecarErrorDto variants
    kind: "not-a-real-kind",
    message: "bogus",
  };
  void e;
}
