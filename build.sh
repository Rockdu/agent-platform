#!/usr/bin/env bash
# Helper script for common build / dev / test workflows.
# Usage: ./build.sh <command>
#
# Commands:
#   install   Install npm dependencies
#   codegen   Run plugin-codegen + build terminal-mesh-sidecar
#   dev       Run the full Tauri app in dev mode (opens window)
#   web       Vite-only dev server (frontend without Tauri)
#   build     Build frontend bundle to dist/
#   app       Build the Tauri desktop app bundle (release)
#   test      Run all workspace cargo tests
#   clippy    Run clippy with -D warnings across the workspace
#   regen     Wipe generated dirs + clean rebuild of front + sidecar
#   clean     Cargo clean + remove dist/ and generated/
#   check     Quick cargo check --workspace
#   help      Show this message

set -euo pipefail

# Ensure cargo + brew tools are on PATH even in non-interactive shells.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

cd "$(dirname "$0")"

cmd="${1:-help}"

case "$cmd" in
  install)
    npm install
    ;;

  codegen)
    cargo run --quiet -p plugin-codegen
    cargo build --quiet -p terminal-mesh-sidecar
    ;;

  dev)
    npm run tauri dev
    ;;

  web)
    npm run dev
    ;;

  build)
    npm run build
    ;;

  app)
    npm run tauri build
    ;;

  test)
    cargo test --workspace
    ;;

  clippy)
    cargo clippy --workspace --all-targets -- -D warnings
    ;;

  regen)
    rm -rf src/generated src-tauri/src/generated
    npm run build
    ;;

  clean)
    cargo clean
    rm -rf dist src/generated src-tauri/src/generated
    ;;

  check)
    cargo check --workspace
    ;;

  help|-h|--help)
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
    ;;

  *)
    echo "Unknown command: $cmd" >&2
    echo "Run: ./build.sh help" >&2
    exit 1
    ;;
esac
