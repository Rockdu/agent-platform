// Compile-time probe for the workspaces wire shape (Round 28 /
// task17 / AC-4.3 + AC-4.4 + AC-9.1). Mirrors
// src-tauri/src/workspaces.rs::{WorkspaceRecord, WorkspaceErrorDto}.

import {
  isWorkspaceErrorDto,
  type WorkspaceErrorDto,
  type WorkspaceRecord,
} from "../src/workspaces";

export function _probe_record_shape(r: WorkspaceRecord): string {
  return [
    r.workspaceId,
    r.name,
    r.path,
    r.createdAt,
    r.lastUsedAt,
    r.openTabId ?? "<closed>",
  ].join("|");
}

export function _probe_error_dto_discrimination(e: WorkspaceErrorDto): string {
  switch (e.kind) {
    case "invalidName":
      return `invalidName:${e.reason}`;
    case "workspaceAlreadyExists":
      return `workspaceAlreadyExists:${e.path}`;
    case "canonicalDuplicate":
      return `canonicalDuplicate:${e.existingWorkspaceId}:${e.existingName}`;
    case "notADirectory":
      return `notADirectory:${e.path}`;
    case "notFound":
      return `notFound:${e.workspaceId}`;
    case "alreadyOpen":
      return `alreadyOpen:${e.existingTabId}`;
    case "io":
      return `io:${e.context}:${e.message}`;
  }
}

export function _probe_is_workspace_error_dto_narrows(): void {
  const v: unknown = { kind: "invalidName", reason: "bad" };
  if (isWorkspaceErrorDto(v)) {
    void v.kind;
  }
}

// Object-literal probes — tsc places the error on the offending field.
export function _probe_invalid_record_missing_path(): void {
  // @ts-expect-error `path` is required on WorkspaceRecord
  const r: WorkspaceRecord = {
    workspaceId: "u",
    name: "n",
    createdAt: "2026-01-01T00:00:00Z",
    lastUsedAt: "2026-01-01T00:00:00Z",
    openTabId: null,
  };
  void r;
}

export function _probe_invalid_error_dto_kind(): void {
  const e: WorkspaceErrorDto = {
    // @ts-expect-error kind must be one of the named WorkspaceErrorDto variants
    kind: "not-a-real-kind",
    message: "bogus",
  };
  void e;
}
