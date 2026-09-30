# ast-index — a fork for PHP and JS/TS projects

A fork of [defendend/Claude-ast-index-search](https://github.com/defendend/Claude-ast-index-search) v3.55.0,
tuned for a large PHP/Bitrix codebase (~10k files) with a JS/Vue frontend (~1.3k files).

**Scope of the plugin.** The Claude Code plugin — skill, command references, `/initialize`, hooks — covers PHP,
JavaScript, TypeScript and Vue only. References and `initialize-*` commands for the other upstream languages
(Kotlin/Java, Swift/ObjC, Dart, Rust, Ruby, C#, Go, Python, Perl, C++, Proto, WSDL…) were removed from it. The
indexer itself still parses every language upstream supports; the upstream README documents those commands.

**Why.** Upstream's index was not reliable enough for refactoring. On PHP:
- it did not see classes in PHPDoc (`@var ItemDto[]`, `@return`, `@throws`);
- it mixed up classes that share a short name across namespaces;
- its shared name filter, built for Kotlin/Java, dropped PHP classes and methods such as
  `Exception`, `Result`, `Error`, `get()`, and it did not see `EO_Product_Collection`-style or
  lower-case class names, snake_case calls, or methods called from their own class;
- it skipped the hidden paths where Bitrix keeps code (`.default/`, `.settings.php`, `.tests/`);
- it silently lost edits made within the same second as the last indexing.

On JavaScript, TypeScript and Vue it matched references by short name only: an import of another export of the
same module passed for a use, `export const` values and `export … from` were not symbols, and nothing followed a
module path.

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

**JavaScript, TypeScript, Vue.** The fork indexes ES-module facts — imports with their names, exports,
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

**Effect.** Sonnet in the main session, the same task before and after: before — `rg` only (PHP) or the plugin
off (JS/TS), after — the fork with its plugin. "Tokens saved" — what the searches returned into the context.
Indicative, not exact.

PHP, ~10k files, 2 runs per variant; before/after — complete answers:

| Task | Before | After | Faster | Tokens saved |
|---|---|---|---|---|
| Rename a class | 2/2 | 2/2 | ×1.1 | 7 % |
| Move a class | 0/2¹ | 2/2 | ×0.4² | 20 % |
| Dead classes in a module | 2/2 | 2/2 | ×26 | 59 % |
| Duplicate classes | 2/2 | 2/2 | ×1.8 | 56 % |
| Method signature change | 1/2³ | 2/2 | ×1.3 | 26 % |
| **Total** | **7/10** | **10/10** | **×4.8** | **42 %** |

Bitrix core as `external:` (D7 `lib` and legacy `classes`), plugin off → on, 2 runs per variant:

| Task | Before | After | Faster | Tokens saved |
|---|---|---|---|---|
| Where a legacy method is defined, and its parent class | 2/2 | 2/2 | ×1.0 | ≈ 0⁴ |
| Our calls to a legacy core method | 2/2 | 2/2 | ×0.9 | −450 %⁵ |
| Core subclasses through intermediate classes | 2/2 | 2/2 | ×2.5 | 40 % |

JS/TS/Vue, 1336 files, 1 run per variant:

| Task | Before | After | Faster | Tokens saved |
|---|---|---|---|---|
| Rename a component and its file | 31/33 files | 33/33 | ×1.2 | −14 %⁶ |
| Dead exports in a directory | 8/8 | 8/8 | ×4.5 | 96 % |
| Move a module | 11/11 edits | 11/11 | ×1.3 | 40 % |

<sub>¹ By default `rg` skips the hidden `.tests/` directory and missed the tests that import the class.<br>
² One fork run took 107 s; without it ×1.4.<br>
³ One `rg` run found 2 of 5 anonymous implementations.<br>
⁴ Both answers under 250 tokens: a class name is a literal string, `rg` finds it at once.<br>
⁵ ~130 → ~720 tokens: `rg` on the literal `CSaleOrder::Update` is exact for a static call; `callers` adds the
calls whose receiver it cannot infer (summarised by receiver, first 10 shown).<br>
⁶ The index answer is longer: `impact` lists all 74 uses of the component in templates.</sub>

The index wins where the answer spans files — subclasses through intermediate classes, every importer of a module,
dead code; where one literal string finds the answer, `rg` is as good. The commands answer in 0.02–0.2 s. JS/TS
answers were checked against a reference resolver: 33 of 33 importers of a component, 137 = 137 dead exports over
the frontend, 11 of 11 move edits; upstream's `usages` found 2 of 5 places of a rename. Without the session-start
rules the skill description alone was picked up in 1 of 3 tasks, with them in 3 of 3.

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
