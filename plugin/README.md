# ast-index Agent Plugin

This directory is the shared payload for agent integrations.

## Supported Surfaces

- Claude Code: `.claude-plugin/plugin.json`, `commands/`, and `skills/`.
- Codex: `.codex-plugin/plugin.json` and `skills/`.
- Cursor: `.cursor-plugin/plugin.json`, `skills/`, `rules/`, and
  `commands-cursor/`.

The Claude commands are intentionally separate from the Cursor command because
they write different project configuration files.

For Claude Code, `/initialize` sets up a PHP, JavaScript/TypeScript or Vue
project: `.ast-index.yaml`, the first index, a check. Nothing is written to
`CLAUDE.md` or `.claude/rules/`: in every project that has an index, the
SessionStart hook gives the agent the usage rules (`hooks/context/rules.md`),
and a PreToolUse hook adds a note when a grep/rg search looks for a code symbol
(`hooks/scripts/search-reminder.py`, at most once per 10 minutes per project,
never a permission decision).

## Local Testing

Codex can load the skill directly from `~/.codex/skills`:

```bash
mkdir -p ~/.codex/skills
ln -s /absolute/path/to/Claude-ast-index-search/plugin/skills/ast-index ~/.codex/skills/ast-index
```

The repo marketplace at `.agents/plugins/marketplace.json` is kept for Codex
builds that support plugin marketplaces.

Cursor can load the skill directly from `~/.cursor/skills`:

```bash
mkdir -p ~/.cursor/skills
ln -s /absolute/path/to/Claude-ast-index-search/plugin/skills/ast-index ~/.cursor/skills/ast-index
```

Cursor can load the plugin locally by symlinking this directory:

```bash
mkdir -p ~/.cursor/plugins/local
ln -s /absolute/path/to/Claude-ast-index-search/plugin ~/.cursor/plugins/local/ast-index
```

Then reload Cursor and verify the `ast-index` skill, rule, and
`initialize-ast-index` command appear.

Claude Code can install the plugin from a local checkout while it is being
changed:

```bash
claude plugin marketplace add /absolute/path/to/Claude-ast-index-search
claude plugin install ast-index@ast-index-php --scope user
```

Hook tests: `scripts/test.sh --test plugin_hooks_tests`.
