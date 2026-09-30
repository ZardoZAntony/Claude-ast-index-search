#!/usr/bin/env python3
"""PreToolUse hook (Grep, Bash): a text search for a code symbol in a project with an index gets a short
note that ast-index answers it precisely.

The note goes as `additionalContext` only. The hook never decides on permission: an "allow" from it would
let a Bash command through without the user's permission rules.

A symbol is a camelCase or PascalCase name (`useSizeGuide`, `OrderDto`), `Type::method`, or a namespaced PHP
name (`App\\Order\\OrderDto`). Plain words (`cron`, `TODO`) and regexes are left alone, and so is grep that
filters another command's output (`ps aux | grep …`). At most one note per project per
AST_INDEX_REMINDER_INTERVAL seconds (default 600). Bypass with AST_INDEX_HOOK_SKIP_REMINDER=1.
"""

import json
import os
import re
import shlex
import subprocess
import sys
import time
import zlib

IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
SYMBOL = re.compile(
    rf"^(?:\\{{0,2}}{IDENT}(?:\\{{1,2}}{IDENT})+"  # App\Order\OrderDto, App\\Order\\OrderDto in a regex
    rf"|{IDENT}::{IDENT}"  # OrderRepository::findById
    r"|[a-z][a-z0-9]*[A-Z][A-Za-z0-9]*"  # camelCase
    r"|[A-Z][a-z0-9]+[A-Z][A-Za-z0-9]*)$"  # PascalCase with two humps or more
)
ANCHORS = re.compile(r"^(?:\\b|\\<|\^)?(.*?)(?:\\b|\\>|\$)?$")

# Options that take a value in the next argument; the pattern is the first positional argument otherwise.
VALUE_OPTIONS = {
    "rg": {"-e", "--regexp", "-f", "--file", "-g", "--glob", "--iglob", "-t", "--type", "-T", "--type-not",
           "-A", "-B", "-C", "-m", "--max-count", "-d", "--max-depth", "-j", "--threads", "-M", "--max-columns",
           "-r", "--replace", "-E", "--encoding", "--type-add", "--sort", "--sortr", "--pre", "--pre-glob"},
    "grep": {"-e", "--regexp", "-f", "--file", "-A", "-B", "-C", "-m", "--max-count", "-d", "--directories",
             "-D", "--devices", "--include", "--exclude", "--exclude-dir", "--label"},
}
SEPARATORS = {"|", "||", "&&", ";", "&", "(", ")", "\n"}


def main() -> None:
    if os.environ.get("AST_INDEX_HOOK_SKIP_REMINDER") == "1":
        return
    try:
        event = json.load(sys.stdin)
    except ValueError:
        return
    tool = event.get("tool_name")
    tool_input = event.get("tool_input") or {}
    if "Grep" == tool:
        symbol = as_symbol(str(tool_input.get("pattern", "")))
    elif "Bash" == tool:
        symbol = bash_search_symbol(str(tool_input.get("command", "")))
    else:
        return
    if symbol is None:
        return

    project_dir = os.environ.get("CLAUDE_PROJECT_DIR") or event.get("cwd") or os.getcwd()
    db_path = index_path(project_dir)
    if db_path is None or throttled(db_path):
        return

    note = (
        f"ast-index: '{symbol}' — code symbol(s), and this project has an index. "
        "PHP: `ast-index usages '<FQN>'`, `ast-index impact '<FQN>'`, `ast-index callers 'Type::method'` give "
        "the complete answer in one call (namespaces, imports, PHPDoc, tests). JS/TS: `ast-index impact "
        "'<file>#<name>'` or `ast-index impact <name>` (imports resolved by path, re-exports, Vue templates, "
        "tests). rg stays right for text that is not code and excluded files."
    )
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": note}},
                     ensure_ascii=False))


def as_symbol(pattern: str):
    """The symbol a pattern searches for; an alternation of symbols (`Foo\\|Bar`, `Foo|Bar`) gives them all."""
    parts = re.split(r"\\?\|", pattern.strip())
    symbols = [one_symbol(part) for part in parts]
    if not symbols or None in symbols:
        return None
    return ", ".join(symbols)


def one_symbol(pattern: str):
    match = ANCHORS.match(pattern.strip())
    candidate = match.group(1) if match else pattern
    # a namespace separator escaped for the regex (`App\\Order`) is shown as PHP writes it
    return candidate.replace("\\\\", "\\") if SYMBOL.match(candidate) else None


def bash_search_symbol(command: str):
    """The searched symbol of the first grep/rg/git grep over files in the command, if any."""
    if "grep" not in command and "rg" not in command:
        return None
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return None

    segment, after_pipe = [], False
    for token in tokens + [";"]:
        if token not in SEPARATORS:
            segment.append(token)
            continue
        symbol = segment_symbol(segment, after_pipe)
        if symbol is not None:
            return symbol
        segment, after_pipe = [], "|" == token
    return None


def segment_symbol(words: list, after_pipe: bool):
    while words and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", words[0]):
        words = words[1:]  # FOO=bar rg …
    if not words:
        return None
    if "git" == words[0] and len(words) > 1 and "grep" == words[1]:
        kind, args, reads_files = "grep", words[2:], True
    elif words[0] in ("rg", "grep", "egrep", "fgrep"):
        kind = "rg" if "rg" == words[0] else "grep"
        args = words[1:]
        reads_files = None  # decided below
    else:
        return None

    patterns, positional, recursive = [], [], False
    value_options = VALUE_OPTIONS[kind]
    index = 0
    while index < len(args):
        arg = args[index]
        index += 1
        if "--" == arg:
            positional.extend(args[index:])
            break
        if arg.startswith("--"):
            name = arg.split("=", 1)[0]
            if name in ("--recursive", "--dereference-recursive"):
                recursive = True
            if "=" not in arg and name in value_options and index < len(args):
                value = args[index]
                index += 1
                if name == "--regexp":
                    patterns.append(value)
            elif name == "--regexp" and "=" in arg:
                patterns.append(arg.split("=", 1)[1])
            continue
        if arg.startswith("-") and len(arg) > 1:
            if "grep" == kind and ("r" in arg[1:] or "R" in arg[1:]):
                recursive = True
            option = "-" + arg[-1]
            if option in value_options and index < len(args):
                value = args[index]
                index += 1
                if "-e" == option:
                    patterns.append(value)
            continue
        positional.append(arg)

    if not patterns and positional:
        patterns.append(positional.pop(0))
    if reads_files is None:
        # rg searches the working directory by default; grep reads files only when given paths or -r
        reads_files = not after_pipe and ("rg" == kind or recursive or bool(positional))
    if not reads_files:
        return None
    for pattern in patterns:
        symbol = as_symbol(pattern)
        if symbol is not None:
            return symbol
    return None


def index_path(project_dir: str):
    try:
        out = subprocess.run(["ast-index", "db-path"], cwd=project_dir, capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return None
    path = out.stdout.strip()
    return path if 0 == out.returncode and path and os.path.isfile(path) else None


def throttled(db_path: str) -> bool:
    try:
        interval = int(os.environ.get("AST_INDEX_REMINDER_INTERVAL", "600"))
    except ValueError:
        interval = 600
    state_dir = os.environ.get("AST_INDEX_REMINDER_STATE_DIR") or os.path.join(
        os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "ast-index", "reminders")
    stamp = os.path.join(state_dir, format(zlib.crc32(db_path.encode()), "08x"))
    try:
        if time.time() - os.path.getmtime(stamp) < interval:
            return True
    except OSError:
        pass
    try:
        os.makedirs(state_dir, exist_ok=True)
        with open(stamp, "w"):
            pass
    except OSError:
        pass
    return False


if __name__ == "__main__":
    main()
