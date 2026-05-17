// Compile-time probe for the terminal-mesh wire shape (Round 25 /
// task16 / AC-4.1 + AC-4.2). Mirrors
// src-tauri/src/terminal_mesh.rs::TerminalMeshErrorDto and the
// TerminalEvent / AttentionKind discriminants in
// crates/terminal-mesh-core/src/events.rs.

import {
  isTerminalMeshErrorDto,
  type AttentionKind,
  type TerminalEvent,
  type TerminalEventEnvelope,
  type TerminalMeshErrorDto,
  type TerminalSpawnResponse,
} from "../src/terminal-mesh";

export function _probe_spawn_response_shape(r: TerminalSpawnResponse): string {
  return r.terminalId;
}

export function _probe_envelope_shape(e: TerminalEventEnvelope): string {
  return `${e.terminalId}|${e.pluginId}|${e.event.kind}`;
}

export function _probe_event_discrimination(e: TerminalEvent): string {
  switch (e.kind) {
    case "output":
      return `output:${e.bytes.length}`;
    case "resize":
      return `resize:${e.cols}x${e.rows}`;
    case "exit":
      return `exit:${e.code ?? "none"}`;
    case "needsAttention":
      return `needsAttention:${e.payload.kind.kind}`;
    case "cancelled":
      return "cancelled";
  }
}

export function _probe_attention_kind_discrimination(k: AttentionKind): string {
  switch (k.kind) {
    case "completion":
      return `completion:${k.exitCode}`;
    case "nonZeroExit":
      return `nonZeroExit:${k.exitCode}`;
    case "promptWaiting":
      return "promptWaiting";
    case "agentMarker":
      return `agentMarker:${k.severity}:${k.summary ?? "<none>"}`;
  }
}

export function _probe_error_dto_discrimination(e: TerminalMeshErrorDto): string {
  switch (e.kind) {
    case "notFound":
      return `notFound:${e.terminalId}`;
    case "invalidTerminalId":
      return `invalidTerminalId:${e.raw}:${e.message}`;
    case "spawn":
      return `spawn:${e.message}`;
    case "io":
      return `io:${e.context}:${e.message}`;
  }
}

export function _probe_is_terminal_mesh_error_dto_narrows(): void {
  const v: unknown = { kind: "notFound", terminalId: "abc" };
  if (isTerminalMeshErrorDto(v)) {
    // narrowed in this branch
    void v.kind;
  }
}

// Object-literal probes: tsc places the error on the offending field.
export function _probe_invalid_event_kind(): void {
  const e: TerminalEvent = {
    // @ts-expect-error kind must be one of the named TerminalEvent variants
    kind: "not-a-real-event",
  };
  void e;
}

export function _probe_invalid_error_dto_kind(): void {
  const e: TerminalMeshErrorDto = {
    // @ts-expect-error kind must be one of the named TerminalMeshErrorDto variants
    kind: "not-a-real-kind",
    message: "bogus",
  };
  void e;
}
