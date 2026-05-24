#!/usr/bin/env node
// Copy release sidecar binaries to the target-triple-suffixed name
// Tauri's `bundle.externalBin` expects. Tauri appends `-<target-triple>`
// to each externalBin entry at bundle time and requires the file to
// exist as a regular non-empty binary. Cargo writes the canonical
// `papers-plugin` (no suffix) so we copy it into place.
//
// Refuses to copy zero-byte placeholders that `build.rs` may have left
// behind from an earlier `cargo check`.

import { execSync } from "node:child_process";
import { existsSync, copyFileSync, statSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, "..");
const SIDECARS = ["terminal-mesh-sidecar", "notes-plugin", "papers-plugin"];

function detectTargetTriple() {
  try {
    const out = execSync("rustc -vV", { encoding: "utf8" });
    const m = out.match(/^host:\s*(\S+)/m);
    if (m) return m[1];
  } catch {
    /* fall through */
  }
  throw new Error(
    "stage-release-sidecars: could not detect host target triple from `rustc -vV`",
  );
}

function isRealBinary(path) {
  if (!existsSync(path)) return false;
  const stat = statSync(path);
  return stat.isFile() && stat.size > 0;
}

const triple = detectTargetTriple();
const releaseDir = resolve(repoRoot, "target", "release");
// On Windows, Cargo writes `<sidecar>.exe` to target/release and
// Tauri's `bundle.externalBin` check expects the staged copy as
// `<sidecar>-<triple>.exe`. Without the suffix on both `src` and
// `dst`, the script reports every sidecar as missing on Windows and
// blocks `tauri build`. `process.platform === "win32"` covers all
// Windows hosts regardless of msvc/gnu/uwp triple.
const exeSuffix = process.platform === "win32" ? ".exe" : "";

let failures = 0;
for (const sidecar of SIDECARS) {
  const src = resolve(releaseDir, `${sidecar}${exeSuffix}`);
  const dst = resolve(releaseDir, `${sidecar}-${triple}${exeSuffix}`);
  if (!isRealBinary(src)) {
    console.error(
      `stage-release-sidecars: ${src} is missing or zero-byte; build it first with \`cargo build --release -p ${sidecar}\``,
    );
    failures += 1;
    continue;
  }
  copyFileSync(src, dst);
  // Verify the copy is non-empty too.
  if (!isRealBinary(dst)) {
    console.error(
      `stage-release-sidecars: copied ${dst} ended up empty (filesystem error?)`,
    );
    failures += 1;
    continue;
  }
  console.log(`stage-release-sidecars: staged ${dst}`);
}

if (failures > 0) {
  process.exit(1);
}
