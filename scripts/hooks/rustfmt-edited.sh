#!/bin/sh
# Claude Code PostToolUse hook: format the one .rs file that was just edited.
#
# Input: hook JSON on stdin; reads `tool_input.file_path`.
# Formats with nightly rustfmt (rustfmt.toml uses unstable options), the same
# toolchain lookup as `just fmt`. A no-op (exit 0, nothing changed) for anything
# that is not an existing .rs file, and when python3, rustup or nightly rustfmt is
# missing. Never fails the tool call.

input=$(cat) || exit 0

command -v python3 >/dev/null 2>&1 || exit 0
file=$(printf '%s' "$input" | python3 -I -c '
import json, sys
try:
    print(json.load(sys.stdin).get("tool_input", {}).get("file_path") or "")
except Exception:
    pass
' 2>/dev/null) || exit 0

case "$file" in
    *.rs) ;;
    *) exit 0 ;;
esac
[ -f "$file" ] || exit 0

command -v rustup >/dev/null 2>&1 || exit 0
rustfmt=$(rustup which --toolchain nightly rustfmt 2>/dev/null) || exit 0
[ -x "$rustfmt" ] || exit 0

root=${CLAUDE_PROJECT_DIR:-$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)}

# --config-path so rustfmt.toml applies even to files outside the repo.
"$rustfmt" --edition 2024 --config-path "$root" "$file" >/dev/null 2>&1 || true
exit 0
