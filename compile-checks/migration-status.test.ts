// Compile-time probe for the PluginMigrationStatus wire shape. Mirrors the
// Rust enum in src-tauri/src/plugin_sqlite.rs; if either the Rust enum or
// the TS mirror drifts, `tsc -b` here is the first failure surface.

import type { PluginMigrationStatus } from "../src/migration-status";

export function _probe_ok(status: PluginMigrationStatus): string | undefined {
  if (status.kind === "ok") {
    return status.applied[0];
  }
  return undefined;
}

export function _probe_error(status: PluginMigrationStatus): string {
  if (status.kind === "error") {
    return `${status.errorKind}:${status.file ?? "<no-file>"}:${status.message}`;
  }
  return "(ok)";
}

export function _probe_exhaustive(status: PluginMigrationStatus): string {
  // Discriminated union: the switch is exhaustive only because both arms are
  // handled. Removing either arm would make `kind` infer a residual type.
  switch (status.kind) {
    case "ok":
      return `applied=${status.applied.length}`;
    case "error":
      return `error=${status.errorKind}`;
  }
}

export function _probe_invalid_shapes(): void {
  // @ts-expect-error missing `applied` for the `ok` variant
  const _bad_ok: PluginMigrationStatus = { kind: "ok" };
  void _bad_ok;

  // @ts-expect-error missing `errorKind`/`message` for the `error` variant
  const _bad_err: PluginMigrationStatus = { kind: "error", file: "x" };
  void _bad_err;

  // @ts-expect-error `kind` must be the literal "ok" or "error"
  const _bad_kind: PluginMigrationStatus = { kind: "warn", applied: [] };
  void _bad_kind;
}
