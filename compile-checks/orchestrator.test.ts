// Compile-time probe for the orchestrator wire shape (Round 35 /
// task20 / AC-3.1 + AC-3.2). Mirrors src-tauri/src/orchestrator.rs::
// {OrchestratorStatus, OrchestratorSession, OrchestratorErrorDto}.

import {
  isOrchestratorErrorDto,
  type OrchestratorErrorDto,
  type OrchestratorSession,
  type OrchestratorStatus,
} from "../src/orchestrator";

export function _probe_session_shape(s: OrchestratorSession): string {
  return `${s.terminalId}|${s.tabId}|${s.mcpConfigPath}`;
}

export function _probe_status_discrimination(s: OrchestratorStatus): string {
  switch (s.kind) {
    case "ready":
      return `ready:${s.session.terminalId}`;
    case "notLaunched":
      return "notLaunched";
    case "claudeMissing":
      return "claudeMissing";
  }
}

export function _probe_error_dto_discrimination(e: OrchestratorErrorDto): string {
  switch (e.kind) {
    case "claudeMissing":
      return "claudeMissing";
    case "mcpConfigFailed":
      return `mcpConfigFailed:${e.message}`;
    case "spawnFailed":
      return `spawnFailed:${e.message}`;
    case "io":
      return `io:${e.context}:${e.message}`;
  }
}

export function _probe_is_orchestrator_error_dto_narrows(): void {
  const v: unknown = { kind: "claudeMissing" };
  if (isOrchestratorErrorDto(v)) {
    void v.kind;
  }
}

export function _probe_invalid_status_kind(): void {
  const s: OrchestratorStatus = {
    // @ts-expect-error kind must be one of the named OrchestratorStatus variants
    kind: "not-a-real-kind",
  };
  void s;
}

export function _probe_invalid_session_missing_path(): void {
  const s: OrchestratorSession = {
    terminalId: "u",
    tabId: "t",
    // @ts-expect-error `mcpConfigPath` must be a string, not a number
    mcpConfigPath: 42,
  };
  void s;
}
