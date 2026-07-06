#!/usr/bin/env bash
# Install the visualize-* skill set so Claude Code can discover it.
# Symlinks each skill into ~/.claude/skills (personal scope: always available).
# Re-runnable; use --copy to hard-copy instead of symlink, --uninstall to remove.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"
DEST="${CLAUDE_SKILLS_DIR:-$HOME/.claude/skills}"
SKILLS=(visualize-anything visualize-repo visualize-pr visualize-agents visualize-agent-trace)
MODE="link"
[[ "${1:-}" == "--copy" ]] && MODE="copy"
[[ "${1:-}" == "--uninstall" ]] && MODE="uninstall"

mkdir -p "$DEST"
for s in "${SKILLS[@]}"; do
  target="$DEST/$s"
  rm -rf "$target"
  case "$MODE" in
    uninstall) echo "removed  $target"; continue ;;
    copy)      cp -R "$SRC/$s" "$target"; echo "copied   $target" ;;
    link)      ln -s "$SRC/$s" "$target"; echo "linked   $target -> $SRC/$s" ;;
  esac
done

if [[ "$MODE" != "uninstall" ]]; then
  echo
  echo "Installed to $DEST. In Claude Code, run /skills to confirm they're listed,"
  echo "or just ask: \"visualize this repo / PR / the agents / this agent's trace\"."
fi
