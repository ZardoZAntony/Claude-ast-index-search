#!/usr/bin/env bash
# Session-start hook: keep the ast-index fresh between sessions.
#
# Behaviour:
#   * No-op when ast-index is not on PATH.
#   * Reports when this project's `ast-index watch` daemon is already active.
#   * If an index exists: queue a coordinated background refresh. Index-reading
#     commands wait for that generation before returning results.
#   * If no index exists: print a one-line hint to stderr so the user runs the
#     initial `ast-index rebuild` themselves; we deliberately don't start
#     a multi-minute rebuild on session start.
#   * If an index exists and no watcher runs: start `ast-index watch` in the
#     background. It catches what edit hooks cannot — git and other shell
#     commands, formatters, edits in an IDE. Opt out with AST_INDEX_HOOK_WATCH=0.
#   * Bypassable via AST_INDEX_HOOK_SKIP_SESSION_START=1 for opt-out.

set -u

if [ "${AST_INDEX_HOOK_SKIP_SESSION_START:-0}" = "1" ]; then
  exit 0
fi

if ! command -v ast-index >/dev/null 2>&1; then
  exit 0
fi

project_dir="${CLAUDE_PROJECT_DIR:-$PWD}"

if (cd "$project_dir" && ast-index watch-status --quiet >/dev/null 2>&1); then
  echo "ast-index: project watcher is active; the index is already kept fresh." >&2
  exit 0
fi

db_path=$(cd "$project_dir" && ast-index db-path 2>/dev/null) || exit 0
if [ -f "$db_path" ]; then
  debounce_ms="${AST_INDEX_SESSION_DEBOUNCE_MS:-0}"
  case "$debounce_ms" in
    ''|*[!0-9]*) debounce_ms=0 ;;
  esac
  if (cd "$project_dir" && ast-index update --background --debounce-ms "$debounce_ms" >/dev/null); then
    echo "ast-index: project index refresh queued; first index read will wait for it." >&2
  else
    echo "ast-index: could not queue refresh; see the diagnostic above." >&2
  fi
  if [ "${AST_INDEX_HOOK_WATCH:-1}" != "0" ]; then
    # Detached from the session: the watcher outlives it and serves every session of the project.
    if command -v setsid >/dev/null 2>&1; then
      (cd "$project_dir" && setsid nohup ast-index watch >/dev/null 2>&1 &)
    else
      (cd "$project_dir" && nohup ast-index watch >/dev/null 2>&1 &)
    fi
    echo "ast-index: started the project watcher (AST_INDEX_HOOK_WATCH=0 to skip)." >&2
  fi
else
  echo "ast-index: no index for $project_dir — run 'ast-index rebuild' to enable structural search." >&2
fi

exit 0
