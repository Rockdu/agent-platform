#!/usr/bin/env bash
# One-shot bootstrap for the Agent Platform (Tauri v2 + Rust + React/TS).
#
# Installs every prerequisite and builds the app from a clean checkout on
# macOS. Idempotent: safe to re-run. Written after a from-scratch bring-up
# where the machine had Node/npm/brew but was missing the Rust toolchain.
#
# Usage:
#   ./scripts/setup.sh                 # install deps + build everything (default)
#   ./scripts/setup.sh --with-claude   # ... also install the Claude Code CLI
#   ./scripts/setup.sh --skip-login    # ... don't launch the claude auth login flow
#   ./scripts/setup.sh --run           # ... then launch the app in dev mode
#   ./scripts/setup.sh --help
#
# Beyond installing, it also logs the `claude` CLI in: if not already
# authenticated (and ANTHROPIC_API_KEY isn't set), it launches the interactive
# `claude auth login` browser flow when a terminal is attached.
#
# What it fixes automatically (the things that blocked a clean bring-up):
#   1. Rust toolchain absent           -> installs via rustup (stable).
#   2. npm 11 blocks install scripts   -> package.json now carries an
#                                         `allowScripts` allowlist for
#                                         esbuild/fsevents; we also re-approve
#                                         any still-pending scripts defensively.
#   3. libsodium-sys-stable build race -> the C `configure` step occasionally
#                                         races under parallel cargo builds
#                                         ("C compiler cannot create
#                                         executables"). We retry the Rust
#                                         build, wiping the corrupt artifact.
#   4. claude installed but logged out -> runs `claude auth login` (interactive)
#                                         so the orchestrator actually works.

set -euo pipefail

RUN_APP=0
WITH_CLAUDE=0
SKIP_LOGIN=0
for arg in "$@"; do
  case "$arg" in
    --run) RUN_APP=1 ;;
    --with-claude) WITH_CLAUDE=1 ;;
    --skip-login) SKIP_LOGIN=1 ;;
    -h|--help)
      sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "Unknown option: $arg" >&2; exit 2 ;;
  esac
done

# Repo root = parent of this script's dir.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Make cargo + Homebrew tools visible even in a non-login shell.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 1. macOS toolchain prerequisites
# ---------------------------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || warn "This script is tuned for macOS; proceed with care."

if ! xcode-select -p >/dev/null 2>&1; then
  warn "Xcode Command Line Tools not found. Launching the installer..."
  xcode-select --install || true
  die "Install the Command Line Tools, then re-run this script."
fi
log "Xcode Command Line Tools: OK"

command -v node >/dev/null || die "Node.js not found. Install Node 20+ (e.g. 'brew install node') and re-run."
command -v npm  >/dev/null || die "npm not found. Install Node/npm and re-run."
log "Node $(node --version), npm $(npm --version)"

# ---------------------------------------------------------------------------
# 2. Rust toolchain (the usual missing piece)
# ---------------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  log "Rust toolchain not found. Installing via rustup (stable)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile default
  # rustup drops cargo here; make it available for the rest of this run.
  export PATH="$HOME/.cargo/bin:$PATH"
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null || die "cargo still not on PATH after install."
log "Rust $(rustc --version)"

# ---------------------------------------------------------------------------
# 3. Frontend dependencies (npm)
# ---------------------------------------------------------------------------
log "Installing npm dependencies..."
npm install

# npm 11+ blocks package install scripts unless allowlisted. package.json
# ships an `allowScripts` block; if a script is still pending (e.g. a newer
# esbuild), approve it and rerun so the native binaries get built.
if npm approve-scripts --allow-scripts-pending 2>/dev/null | grep -q 'not yet covered'; then
  warn "Approving pending install scripts (esbuild/fsevents)..."
  npm approve-scripts esbuild  >/dev/null 2>&1 || true
  npm approve-scripts fsevents >/dev/null 2>&1 || true
  npm install
fi

# Sanity-check the esbuild native binary is present & runnable.
if ! ./node_modules/.bin/esbuild --version >/dev/null 2>&1; then
  die "esbuild binary missing/broken. Run 'npm approve-scripts esbuild && npm install'."
fi
log "npm dependencies: OK"

# ---------------------------------------------------------------------------
# 4. Rust codegen + plugin sidecars
# ---------------------------------------------------------------------------
log "Running plugin codegen..."
cargo run --quiet -p plugin-codegen

log "Building plugin sidecars (debug)..."
cargo build --quiet -p terminal-mesh-sidecar -p notes-plugin -p papers-plugin

# ---------------------------------------------------------------------------
# 5. Build the Tauri app binary (with libsodium-race retry)
# ---------------------------------------------------------------------------
# `tauri dev` compiles the app with --no-default-features; do the same here so
# the heavy compile is cached before launch. libsodium-sys-stable (pulled in by
# tauri-plugin-stronghold) builds vendored C via ./configure and can race under
# parallel builds. On failure, wipe its artifact and retry once serially.
build_app() {
  cargo build --no-default-features -p agent-platform
}
log "Building the Tauri app (this is the long one on a cold cache)..."
if ! build_app; then
  warn "App build failed — clearing libsodium artifact and retrying serially..."
  rm -rf target/debug/build/libsodium-sys-stable-*
  CARGO_BUILD_JOBS=1 build_app \
    || die "App build failed again. See the cargo output above."
fi
log "Build complete."

# ---------------------------------------------------------------------------
# 5b. Runtime dependency: the `claude` CLI (the app's core orchestrator agent)
# ---------------------------------------------------------------------------
# Not needed to build or launch — the app boots without it and shows an
# onboarding card — but the orchestrator tab is inert until `claude` is found.
# Discovery probes /opt/homebrew/bin, /usr/local/bin, ~/.local/bin,
# ~/.npm-global/bin, then `bash -lc 'command -v claude'`. A global npm install
# lands in `$(npm prefix -g)/bin`, which on Homebrew Node is /opt/homebrew/bin
# — the first probed path — so the app auto-discovers it on next launch.
claude_present() { command -v claude >/dev/null 2>&1 || bash -lc 'command -v claude' >/dev/null 2>&1; }

CLAUDE_READY=0
if claude_present; then
  log "claude CLI: found ($(claude --version 2>/dev/null | head -1))"
  CLAUDE_READY=1
elif [ "$WITH_CLAUDE" -eq 1 ]; then
  log "Installing Claude Code CLI globally via npm..."
  # npm 11 blocks the package's postinstall (it fetches the native binary)
  # unless explicitly allowed for this install.
  npm install -g --allow-scripts=@anthropic-ai/claude-code @anthropic-ai/claude-code
  if claude_present; then
    log "claude CLI: $(claude --version 2>/dev/null | head -1)"
    CLAUDE_READY=1
  else
    warn "claude install ran but the binary isn't resolving — check '\$(npm prefix -g)/bin' is on PATH."
  fi
else
  warn "claude CLI not found — the app runs, but the orchestrator agent will be inert."
  warn "  Install it with:  ./scripts/setup.sh --with-claude"
  warn "  (or manually:     npm install -g --allow-scripts=@anthropic-ai/claude-code @anthropic-ai/claude-code)"
  warn "  Or point the app at an existing binary via its in-app onboarding card."
fi

# ---------------------------------------------------------------------------
# 5c. Log the `claude` CLI in (installing it isn't enough — it needs auth).
# ---------------------------------------------------------------------------
# Auth is inherently interactive: `claude auth login` opens a browser OAuth
# flow. A headless script can't complete that, so we drive it as far as we
# can: detect existing auth, honor ANTHROPIC_API_KEY, launch the interactive
# login when a terminal is attached, and print instructions when it isn't.
claude_logged_in() {
  # `claude auth status --json` -> {"loggedIn": true/false, ...}; exit 0 always.
  claude auth status --json 2>/dev/null | grep -q '"loggedIn"[[:space:]]*:[[:space:]]*true'
}
if [ "$CLAUDE_READY" -eq 1 ]; then
  if claude_logged_in; then
    log "claude auth: already logged in."
  elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    log "claude auth: ANTHROPIC_API_KEY is set — API-key auth will be used (no login needed)."
  elif [ "$SKIP_LOGIN" -eq 1 ]; then
    warn "claude auth: not logged in (--skip-login given). Run 'claude auth login' before using the orchestrator."
  elif [ -t 0 ] && [ -t 1 ]; then
    log "claude auth: not logged in. Launching interactive login (a browser will open)..."
    # Don't let a cancelled/failed login abort the whole setup.
    claude auth login || warn "claude auth login did not complete; run 'claude auth login' later."
    claude_logged_in && log "claude auth: login successful." \
                     || warn "claude auth: still not logged in — run 'claude auth login' when ready."
  else
    warn "claude auth: not logged in and no terminal attached (non-interactive run)."
    warn "  Finish auth with one of:"
    warn "    claude auth login            # browser OAuth (Claude subscription)"
    warn "    claude auth login --console  # Anthropic Console (API billing)"
    warn "    export ANTHROPIC_API_KEY=... # API-key auth, no login"
  fi
fi

echo
log "Setup finished successfully."
echo "  Run the desktop app:      npm run tauri dev   (or ./scripts/setup.sh --run)"
echo "  Frontend only (browser):  npm run dev         -> http://localhost:1420"
echo "  Release bundle:           npm run tauri build"
echo
echo "  Runtime deps (neither blocks build/launch):"
echo "    - claude CLI:  the orchestrator agent. Install: --with-claude."
echo "    - IDE handoff: 'Open in IDE' defaults to Cursor but is overridable"
echo "      in-app to VS Code ('code') / Zed ('zed') / any CLI. Optional."

# ---------------------------------------------------------------------------
# 6. Optional: launch
# ---------------------------------------------------------------------------
if [ "$RUN_APP" -eq 1 ]; then
  log "Launching the app in dev mode..."
  exec npm run tauri dev
fi
