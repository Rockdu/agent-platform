// Wire shape mirroring src-tauri/src/dev_diagnostics.rs::SidecarBinaryDiagnostic.
// Kept hand-written until a Rust-to-TS schema codegen task lands.

export type SidecarBinaryStatus = "present" | "missing";

export interface SidecarBinaryDiagnostic {
  pluginId: string;
  commandBin: string;
  expectedPaths: string[];
  status: SidecarBinaryStatus;
}
