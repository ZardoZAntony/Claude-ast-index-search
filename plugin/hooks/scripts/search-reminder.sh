#!/usr/bin/env bash
# PreToolUse hook (Grep, Bash): see search-reminder.py. Silent without python3 or ast-index; Bash calls that
# mention neither grep nor rg return before python starts.

set -u

command -v ast-index >/dev/null 2>&1 || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

input="$(cat)"
case "$input" in
  *'"Grep"'*|*grep*|*rg*) ;;
  *) exit 0 ;;
esac

printf '%s' "$input" | python3 "$(dirname "$0")/search-reminder.py"
exit 0
