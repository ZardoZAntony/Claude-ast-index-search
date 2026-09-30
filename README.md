# ast-index — a fork for PHP and JS/TS projects

A fork of [defendend/Claude-ast-index-search](https://github.com/defendend/Claude-ast-index-search) v3.55.0,
tuned for a large PHP/Bitrix codebase (~10k files) with a JS/Vue frontend (~1.3k files).

**Scope of the plugin.** The Claude Code plugin — skill, command references, `/initialize`, hooks — covers PHP,
JavaScript, TypeScript and Vue only. References and `initialize-*` commands for the other upstream languages
(Kotlin/Java, Swift/ObjC, Dart, Rust, Ruby, C#, Go, Python, Perl, C++, Proto, WSDL…) were removed from it. The
indexer itself still parses every language upstream supports; the upstream README documents those commands.

**Why.** On PHP, upstream's index was not reliable enough for refactoring:
- it did not see classes in PHPDoc (`@var ItemDto[]`, `@return`, `@throws`);
- it mixed up classes that share a short name across namespaces;
- its shared name filter, built for Kotlin/Java, dropped PHP classes and methods such as
  `Exception`, `Result`, `Error`, `get()`, and it did not see `EO_Product_Collection`-style or
  lower-case class names, snake_case calls, or methods called from their own class;
- it skipped the hidden paths where Bitrix keeps code (`.default/`, `.settings.php`, `.tests/`);
- it silently lost edits made within the same second as the last indexing.

Agents still had to re-check every result with `rg`, so the index only added cost.

**What this fork does.** PHP class references are resolved to fully qualified names (FQN), the way PHP resolves
them. On top of that, new commands answer typical refactoring questions in one call.

| Before | After |
|---|---|
| References by short name: five different `OrderDto` classes look like one | FQN resolution via `namespace`/`use` (aliases, group use), PHPDoc, FQN strings, attributes; `usages`/`implementations` accept an FQN |
| Rename, move, dead code, duplicates, signature change take dozens of greps | `impact <FQN>`, `move-plan <FQN> <namespace>`, `unused-symbols --module … --export-only`, `duplicates`, `callers 'Type::method'` (infers receiver types) |
| PHPDoc is not indexed | PHPDoc tag types are references, just like code |
| Words in SQL strings, heredocs, comments and inline HTML, enum cases, named arguments and constants pass for classes | A per-file lexer resolves names in code only; keywords are case-insensitive; each reference keeps its kind (`import`, `trait-use`, `new`, `type`, …) |
| Kotlin/Java naming rules: `Exception`, `Result`, `get()` dropped as noise; `EO_*`/lower-case classes, snake_case and same-class calls, callables `[Foo::class, 'handle']` unseen | PHP reserved words are the only noise; these names are references |
| Hidden paths are always skipped | `include_hidden` in `.ast-index.yaml` |
| Same-second edits are lost; non-UTF-8 files and directories like `auth.mts/` are re-parsed on every `update` | Nanosecond mtimes; lossy decoding of non-UTF-8 sources; `update` walks files only |
| Minified JS/CSS clutters search | Detected and not parsed |

Details: [PHP commands reference](plugin/skills/ast-index/references/php-commands.md).

**JavaScript, TypeScript, Vue.** Upstream matched JS/TS references by short name only: an import of another
export of the same module passed for a use, `export const` values and `export … from` were not symbols, and
nothing followed a module path. The fork indexes ES-module facts — imports with their names, exports,
re-exports, `import()`, `require()`, `vi.mock`/`jest.mock`, `import.meta.glob`, JSDoc `import()` types, and
uses of imported names in code, TS types and Vue templates (PascalCase and kebab-case tags). Specifiers are
resolved at query time like bundlers do: relative paths, `paths` of the nearest tsconfig/jsconfig (with
`baseUrl`, `extends`, `references`), `js_aliases` from `.ast-index.yaml`, extensions, `index.*`.

| Task | Command |
|---|---|
| Rename an export or change its signature | `impact 'src/x.js#name'` (`#default` for a default export; a short name works when one module exports it) |
| Who imports a module | `impact 'src/x.js'` |
| Move a file | `move-plan 'src/x.js' 'src/y/'` — specifiers per file in the style each was written |
| Dead exports and files | `unused-symbols --module src/` |

Details: [JS/TS reference](plugin/skills/ast-index/references/typescript-commands.md).

**Framework and vendor code.** Directories listed under `external:` in `.ast-index.yaml` (Bitrix core
`bitrix/modules/*/lib`, `vendor/symfony`…) are indexed past `.gitignore` for their definitions only: `class`,
`symbol`, `file`, `outline`, `hierarchy` find them, `impact`/`callers` show their definitions and declarations,
`implementations` follows inheritance through them. `search`, `usages`, `unused-symbols`, `duplicates` and grep
leave them out unless `--external`.

**The agent picks the index by itself.** At session start the plugin puts a short list of rules into the context
(only in projects that have an index); a note is added when a `grep`/`rg` search looks for a code symbol (never a
permission decision). Nothing is written to `CLAUDE.md` or `.claude/rules/`.

**Effect on PHP** (measured on a ~10k-file PHP project; Sonnet, 2 runs per variant — indicative, not exact).

One command answers the question (measured before the lexer and naming fixes; `callers` lists
calls it could not infer for a manual check):

| Task | Command | Time | Answer size |
|---|---|---|---|
| Rename a class | `impact <FQN>` | 0.17 s | ~1.9k tokens |
| Move a class | `move-plan <FQN> <namespace>` | 0.17 s | ~0.9k tokens |
| Dead classes in a module | `unused-symbols --module … --export-only` | 0.02 s | ~0.4k tokens |
| Duplicate classes | `duplicates` | 0.04 s | ~5.8k tokens |
| Method signature change | `callers 'Type::method'` | 0.06 s | ~2.5k tokens |

Working in the main session without subagents, this fork vs `rg` only:

| Task | Context growth | Search tokens | Time | Complete answers, `rg` → fork |
|---|---|---|---|---|
| Rename a class | −7 % | −7 % | −13 % | 2/2 → 2/2 |
| Move a class | −20 % | −20 % | +123 %¹ | 0/2 → 2/2² |
| Dead classes in a module | −60 % | −59 % | −96 % (671 → 26 s) | 2/2 → 2/2 |
| Duplicate classes | −52 % | −56 % | −44 % | 2/2 → 2/2 |
| Method signature change | −25 % | −26 % | −25 % | 1/2 → 2/2³ |
| **Average** | **−39 %** | **−42 %** | **−79 %** | **7/10 → 10/10** |

Cost per task is −36 % on average.

<sub>¹ One fork run took 107 s; without it the fork was faster (20 s vs 28 s).<br>
² By default `rg` skips the hidden `.tests/` directory, so both `rg` runs missed the tests that import the class.<br>
³ One `rg` run found 2 of 5 anonymous implementations.</sub>

**Effect on JavaScript, TypeScript, Vue** (a Vue/JS frontend of 1336 files and 4464 imports). Answers were
checked against a reference resolver written for the purpose; before — the same command on upstream's index.

| Task | Command | Time | Before | After |
|---|---|---|---|---|
| Rename an exported function | `impact 'file#name'` | 0.03 s | 2 of 5 places (`usages <name>`) | 4 of 4 places + the name line of a multi-line import |
| Who imports a component | `impact 'file'` | 0.03 s | 104 short-name lines, namesakes and template tags mixed | 33 of 33 imports |
| Dead exports in a directory | `unused-symbols --module …` | 0.03 s | 0 of 8 found; template handlers reported as dead | 8 of 8; whole frontend 137 = 137 |
| Move a module | `move-plan file dir/` | 0.03 s | — | 11 of 11 specifiers rewritten in their own style |

11 of 2704 project imports do not resolve: 5 point to missing files, 6 go through a test-runner alias — listed in
`js_aliases`, they resolve too.

**Does the agent use it** (Sonnet, tasks worded in Russian without mentioning the index; 1 run per variant):

| Task | Plugin off | Plugin on |
|---|---|---|
| PHP: remove a DTO field | hooks off, skill on: grep | `class`, then `usages <FQN>`: $0.23 |
| PHP: change a method signature | hooks off, skill on: the skill loaded by itself, `callers` | `class`, then `callers`: $0.20 |
| JS: rename a composable | hooks off, skill on: grep | `usages` + `rg` on the module path: $0.15 |
| JS: rename a component and its file | 31 of 33 files (two tests missed), $0.29 | 33 of 33, $0.30 |
| JS: dead exports in a directory | 8 of 8, $0.28 | 8 of 8, $0.14 |
| JS: move a module | 11 of 11, $0.16 | 11 of 11, $0.17 |
| Bitrix core: how `HttpClient::download()` works | $0.18 | $0.19 — the core path is guessable, grep in one file is enough |

Without the session-start rules the skill description alone was picked up in 1 of 3 PHP/JS tasks. Suggesting
same-named classes for a guessed namespace cut the PHP sessions from $0.35/$0.33 to $0.23/$0.20.

## Install

1. Get the binary.

   Linux x86_64 with glibc 2.39 or newer (Ubuntu 24.04+, WSL included; check with
   `ldd --version`) — download the release build into a directory on `PATH`:

   ```bash
   mkdir -p ~/.local/bin
   curl -fsSL https://github.com/ZardoZAntony/Claude-ast-index-search/releases/latest/download/ast-index-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin
   ast-index version
   ```

   The same command updates it. Elsewhere, build it (requires Rust):

   ```bash
   cargo install --locked --git https://github.com/ZardoZAntony/Claude-ast-index-search ast-index
   ```

2. Add the Claude Code plugin — the skill with command recipes and the hooks that keep the index
   fresh:

   ```bash
   claude plugin marketplace add ZardoZAntony/Claude-ast-index-search
   claude plugin install ast-index@ast-index-php
   ```

   Or run `/initialize` in the project: it proposes `.ast-index.yaml` (excludes, hidden paths, entry
   points, `js_aliases`, `external`) and builds the index. The usage rules come from the plugin at
   session start — nothing is written to `CLAUDE.md` or `.claude/rules/`.

3. Put `.ast-index.yaml` in the project root (example below) and build the index once:
   `ast-index rebuild`.

### Keeping the index fresh

An index that misses a change gives a confident wrong answer, so every way files change needs
to reach it:

| Files change through | Kept fresh by |
|---|---|
| Anything between sessions | plugin hook at session start: an incremental update; the first query waits for it |
| A new ast-index version that extracts more (JS/TS module facts) | the index migration marks those files, the next update re-parses them once |
| Claude's Edit/Write | plugin hook after each edit |
| Shell commands (`git switch/pull/merge/rebase/stash`, `sed -i`, formatters, generators), your IDE | `ast-index watch`, which the session-start hook starts in the background once per project (`AST_INDEX_HOOK_WATCH=0` to opt out); `ast-index watch-status` tells whether it runs |

There is no hook on every shell command on purpose: queries wait for a queued update, so an
update after each command would delay the next query by seconds. The watcher reacts only to
files the index covers (it honours `.gitignore`, `exclude` and hidden-path rules) and costs no
CPU while nothing changes. On Linux it needs an inotify watch per directory; for large trees
raise `fs.inotify.max_user_watches`. Without the plugin (other agents, CI), run
`ast-index update` after changing files, or keep `ast-index watch` running.

### Configuration

Example `.ast-index.yaml` for Bitrix, in the project root. Paths are gitignore-style patterns:

```yaml
# Hidden paths to index anyway (every other dot-file and dot-directory is skipped):
# component templates, module DI wiring, PHPUnit tests.
include_hidden:
  - .default
  - .settings.php
  - .tests

# Paths left out of the index: styles (selectors are not code symbols), ORM annotation files
# generated by Bitrix (thousands of EO_* stubs), third-party libraries.
exclude:
  - "*.css"
  - "*.scss"
  - "*.less"
  - orm_annotation.php
  - orm_annotations.php
  - vendor/

# unused-symbols never reports symbols of these files: entry points the framework
# calls by convention (a module installer's DoInstall, InstallDB, …).
unused_ignore:
  - "**/install/index.php"

# unused-symbols never reports methods with these names (`*` is a wildcard): controller
# actions are called by the router, ORM overrides by Bitrix itself.
unused_ignore_names:
  - "*Action"
  - getObjectClass
  - getCollectionClass

# duplicates treats pairs under these path fragments as deliberate copies (DTO contracts
# mirrored in modules): tagged [mirror] and listed last.
duplicate_mirrors:
  - /Contracts/
# JS/TS: specifier prefixes a bundler or test runner adds beyond tsconfig/jsconfig `paths`
# (vite/vitest `resolve.alias`), mapped to project-relative directories.
js_aliases:
  "@frontend-ui/": local/frontend/src/ui/
# Framework and vendor code, indexed past .gitignore for definitions only: class/symbol/outline/hierarchy
# and definitions in impact/callers find it; search, usages, unused-symbols, duplicates leave it out
# unless --external.
external:
  - bitrix/modules/main/lib
  - local/vendor/symfony
```

The rest of the tool — installation options, all commands, supported languages, IDE plugins — is documented
in the [upstream README](https://github.com/defendend/Claude-ast-index-search#readme).
