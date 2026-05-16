// Compile-time probe for the dev-diagnostics wire shape (Round 13 / task8 /
// AC-9.2). Mirrors src-tauri/src/dev_diagnostics.rs::SidecarBinaryDiagnostic
// — if either the Rust enum or the TS mirror drifts, `tsc -b` here is the
// first failure surface.

import type {
  SidecarBinaryDiagnostic,
  SidecarBinaryStatus,
} from "../src/dev-diagnostics";

export function _probe_diagnostic_shape(d: SidecarBinaryDiagnostic): string {
  return `${d.pluginId}|${d.commandBin}|${d.expectedPaths[0] ?? "<none>"}|${d.status}`;
}

export function _probe_status_narrowing(d: SidecarBinaryDiagnostic): string {
  switch (d.status) {
    case "present":
      return "present";
    case "missing":
      return `missing:${d.commandBin}`;
  }
}

function _takeStatus(_s: SidecarBinaryStatus): void {}

export function _probe_status_is_literal(): void {
  _takeStatus("present");
  _takeStatus("missing");
  // @ts-expect-error status must be the literal "present" | "missing"
  _takeStatus("unknown");
}

// For object-literal probes, tsc attributes the type-mismatch error to the
// specific field row, so the `@ts-expect-error` directive sits on the line
// directly above the offending field — not on the wrapping call site.
export function _probe_invalid_diagnostic_status(): void {
  const d: SidecarBinaryDiagnostic = {
    pluginId: "x",
    commandBin: "y",
    expectedPaths: [],
    // @ts-expect-error status must be the literal "present" | "missing"
    status: "unknown",
  };
  void d;
}

export function _probe_invalid_diagnostic_expected_paths(): void {
  const d: SidecarBinaryDiagnostic = {
    pluginId: "x",
    commandBin: "y",
    // @ts-expect-error expectedPaths must be string[], not string
    expectedPaths: "/single/path/not/array",
    status: "missing",
  };
  void d;
}
