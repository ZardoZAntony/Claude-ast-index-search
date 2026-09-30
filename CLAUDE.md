# ast-index — Contributor Guide for AI Coding Agents

Fast multi-language code-search CLI written in Rust. Builds a SQLite index of
symbols, references, and file paths for ~30 languages via tree-sitter, then
answers queries in milliseconds.

`.claude/rules/*.md` is loaded into every Claude Code session automatically —
read those for focused guidance (architecture, commands, parsers, commits,
testing, release). This file is the starting point.

## Tech stack

- **Language:** Rust (edition 2021), MSRV tracks stable.
- **Parsers:** tree-sitter grammars (`tree-sitter-<lang>` crates) + regex
  fallbacks for Perl, WSDL/XSD, Vue/Svelte scripts.
- **Storage:** SQLite via `rusqlite`, cache at `~/Library/Caches/ast-index/<hash>/`
  on macOS and `$XDG_CACHE_HOME/ast-index/<hash>/` on Linux.
- **CLI:** `clap` derive with a single global `--format {text,json}` flag.
- **Workspace:** two crates — root `ast-index` (library + binary),
  `crates/ast-index-mcp` (MCP server). Default build produces only the root
  binary; the MCP crate is opt-in via `cargo build -p ast-index-mcp`.

## Folder layout

```
src/
  main.rs              # clap CLI + dispatch to commands::*
  lib.rs               # re-exports db, indexer, parsers, commands
  db.rs                # SQLite schema, migrations, queries
  indexer.rs           # filesystem walk + dispatch to parsers + writes
  commands/
    mod.rs             # shared helpers: PathResolver, search_files, relative_path
    {index,files,grep,management,modules,analysis,project_info}.rs
    {android,ios,perl}.rs  # ecosystem-specific commands
  parsers/
    mod.rs             # FileType enum, parse_file_symbols entry point
    treesitter/
      mod.rs           # LanguageParser trait + get_treesitter_parser()
      {kotlin,swift,...}.rs   # one file per language
      queries/<lang>.scm      # tree-sitter S-expression queries
    {perl,typescript,wsdl}.rs # regex fallbacks
crates/ast-index-mcp/  # MCP server (spawns `ast-index <sub>` per tool call)
tests/                 # integration tests, one *_tests.rs file per concern
scripts/bump.sh        # version bump — never edit versions by hand
```

## Build / test / run

```bash
cargo build --release              # root binary only (default members)
cargo build --release --workspace  # root + MCP
scripts/test.sh                    # full suite, ~1150 tests, ~15 s
./target/release/ast-index search Foo
```

No separate linter step — `cargo clippy --workspace -- -D warnings` before a
PR is welcome but not enforced by CI yet.

## Core invariants (break these and things silently rot)

1. **Paths in the DB are stored relative to whichever root owns the file**
   (primary project root or an `add-root` extra root). Never store absolute
   paths; always resolve for display via `commands::PathResolver`.
2. **`--format json` is per-command.** Only commands whose `cmd_*` signature
   takes `format: &str` honour it; others print plain text regardless. Don't
   branch on format in commands that don't accept the arg.
3. **No global `--verbose`.** Verbose is an explicit per-command flag on
   `rebuild` and `update`. Timing output (`Time: …`, `Total time: …`) only
   prints when that flag is set — default output must stay quiet.
4. **stderr vs stdout.** Query results, help text, JSON payloads → stdout.
   Diagnostics, progress, timings, warnings → stderr. MCP captures stdout
   only; polluting stdout with progress breaks the MCP contract.
5. **Releases go through `scripts/bump.sh X.Y.Z-php.N`.** It sets the version
   in Cargo.toml and the plugin manifests, builds and tests, commits, tags,
   pushes to the branch's upstream and publishes the GitHub release with the
   Linux x86_64 binary (`.claude/rules/release.md`). Hand-edits drift.

## Code style (the short version)

- Prefer `?` over `unwrap`/`expect` in library and command code. `expect` is
  acceptable only for invariants that are compile-time-provable (regex/query
  compilation in a `LazyLock`).
- No comments that describe WHAT the code does — names should. Comments
  explain WHY: a non-obvious invariant, a workaround, a hidden constraint.
- No trailing summaries in commit messages, no `Co-Authored-By`, no emoji.
  One line. See `.claude/rules/commits.md`.
- `colored::Colorize` for human output only — JSON mode must emit uncolored
  output (clap sets `NO_COLOR`-ish behavior implicitly, but don't rely on
  it; branch on `format == "json"` first and return before any colored
  `println!`).
- Never use destructive git ops (`push --force`, `tag -d`, `reset --hard`,
  `branch -D`) without an explicit ask from the user.

## When in doubt

- Adding a command? Read `.claude/rules/commands.md`.
- Adding a language? Read `.claude/rules/parsers.md`.
- Writing tests? Read `.claude/rules/testing.md`.
- Verifying before claiming done? Read `.claude/rules/verify.md`.
- Committing? Read `.claude/rules/commits.md`.
- Cutting a release? Read `.claude/rules/release.md`.
