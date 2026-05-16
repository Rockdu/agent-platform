// Wire shape mirroring src-tauri/src/plugin_sqlite.rs::PluginMigrationStatus.
// Kept hand-written until a Rust-to-TS schema codegen task lands.

export type PluginMigrationStatus =
  | { kind: "ok"; applied: string[] }
  | {
      kind: "error";
      errorKind: string;
      file: string | null;
      message: string;
    };

export type PluginMigrationStatusMap = Record<string, PluginMigrationStatus>;
