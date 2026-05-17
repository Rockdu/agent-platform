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
    case "permissionDenied":
      return `permissionDenied:${e.message}`;
  }
}

// task21 / AC-3.3 negative serialization probe: the wire shape of
// TerminalMeshErrorDto names the denial `permissionDenied`. It must
// NOT carry a `crossTabRead`-flavored field; the privileged flag
// stays in Rust-side MountEntry.
export function _probe_permission_denied_has_no_cross_tab_read_field(): void {
  const e: TerminalMeshErrorDto = {
    kind: "permissionDenied",
    message: "capability lacks cross-tab read privilege",
    // @ts-expect-error `crossTabRead` is not part of the wire shape
    crossTabRead: true,
  };
  void e;
}

// task21 / AC-3.3 Round 39: TerminalSpawnRequest must accept an
// optional `tabId` so workspace tabs can register themselves under
// their tab id (the host RPC bridge resolves `target_tab_id →
// terminal_id` via this index). Omitting `tabId` must still type-
// check (back-compat for the orchestrator path that uses
// `existingTerminalId` instead).
export function _probe_spawn_request_accepts_optional_tab_id(): void {
  const withTab: import("../src/terminal-mesh").TerminalSpawnRequest = {
    cols: 80,
    rows: 24,
    tabId: "tab-A",
  };
  const withoutTab: import("../src/terminal-mesh").TerminalSpawnRequest = {
    cols: 80,
    rows: 24,
  };
  void withTab;
  void withoutTab;
}

export function _probe_spawn_request_tab_id_must_be_string(): void {
  const r: import("../src/terminal-mesh").TerminalSpawnRequest = {
    cols: 80,
    rows: 24,
    // @ts-expect-error `tabId` must be a string when present
    tabId: 42,
  };
  void r;
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
