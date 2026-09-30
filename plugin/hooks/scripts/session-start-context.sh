#!/usr/bin/env bash
# Session-start hook: tell the agent how to use the index — only in projects that have one.
#
# Plain stdout of a SessionStart hook is added to the model context. The text lives in
# hooks/context/rules.md; the refresh hook stays separate and asynchronous.
# Bypass with AST_INDEX_HOOK_SKIP_CONTEXT=1.

set -u

[ "${AST_INDEX_HOOK_SKIP_CONTEXT:-0}" = "1" ] && exit 0
command -v ast-index >/dev/null 2>&1 || exit 0

project_dir="${CLAUDE_PROJECT_DIR:-$PWD}"
db_path=$(cd "$project_dir" && ast-index db-path 2>/dev/null) || exit 0
[ -f "$db_path" ] || exit 0

cat "$(dirname "$0")/../context/rules.md"
exit 0
