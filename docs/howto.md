# HOWTO: working with ast-index

[Русская версия](howto.ru.md)

Task-by-task guide to this fork: PHP (Bitrix included), JavaScript, TypeScript, Vue. Every command is run from the
project root; `--format json` gives the same answer for a script. Command details: the
[PHP reference](../plugin/skills/ast-index/references/php-commands.md) and the
[JS/TS reference](../plugin/skills/ast-index/references/typescript-commands.md).

- [Install and build the index](#install-and-build-the-index)
- [Keep the index fresh](#keep-the-index-fresh)
- [Configure the project](#configure-the-project)
- [Find code](#find-code)
- [PHP: refactor safely](#php-refactor-safely)
- [Bitrix: routes, events, registries, the core](#bitrix-routes-events-registries-the-core)
- [JavaScript, TypeScript, Vue](#javascript-typescript-vue)
- [Frontend to backend: who calls an endpoint](#frontend-to-backend-who-calls-an-endpoint)
- [When rg is the better tool](#when-rg-is-the-better-tool)
- [Agents and scripts](#agents-and-scripts)
- [Troubleshooting](#troubleshooting)

## Install and build the index

```bash
# Linux x86_64, glibc 2.39+ (Ubuntu 24.04+, WSL): the release binary
mkdir -p ~/.local/bin
curl -fsSL https://github.com/ZardoZAntony/Claude-ast-index-search/releases/latest/download/ast-index-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin
ast-index version

# elsewhere: build it (Rust)
cargo install --locked --git https://github.com/ZardoZAntony/Claude-ast-index-search ast-index
```

Claude Code plugin — the skill, the session rules, the hooks that keep the index fresh:

```bash
claude plugin marketplace add ZardoZAntony/Claude-ast-index-search
claude plugin install ast-index@ast-index-php
```

Then, in the project: `/initialize` (proposes `.ast-index.yaml` and builds the index) or by hand:

```bash
ast-index rebuild      # the whole index, seconds to a minute
ast-index stats        # is there an index, what is in it
```

The index lives outside the project (`~/.cache/ast-index/<hash>/` on Linux, `~/Library/Caches/ast-index/<hash>/` on
macOS); `ast-index clear` removes it.

## Keep the index fresh

An index that missed a change answers confidently and wrongly.

| Files change through | What keeps the index fresh |
|---|---|
| Edits between sessions | the plugin runs `update` at session start; the first query waits for it |
| Claude's Edit/Write | the plugin hook after each edit |
| `git switch/pull/rebase`, generators, formatters, the IDE | `ast-index watch` — the plugin starts one per project; `ast-index watch-status` tells whether it runs |
| No plugin (another agent, CI) | `ast-index update` after the change, or a running `ast-index watch` |

After installing a new ast-index version stop the running watchers (`pkill -f '^ast-index watch'`): they keep the
old binary. On Linux a large tree may need a higher `fs.inotify.max_user_watches` for `watch`.

## Configure the project

`.ast-index.yaml` in the project root; paths are gitignore-style patterns.

| Key | What for |
|---|---|
| `exclude` | Leave out styles, generated files, third-party code |
| `include_hidden` | Hidden paths with real code (Bitrix `.default/`, `.settings.php`, `.tests/`) — every other dot path is skipped |
| `unused_ignore`, `unused_ignore_names` | Files and method names the framework calls by convention (installers, `*Action`) — never reported as dead |
| `duplicate_mirrors` | Path fragments of deliberate copies (`/Contracts/`) — `duplicates` lists them last |
| `js_aliases` | Specifier prefixes a bundler or test runner adds beyond `tsconfig`/`jsconfig` `paths` (`"@ui/": src/ui/`) |
| `external` | Framework and vendor directories (`bitrix/modules/main/lib`, `vendor/symfony`) indexed for definitions only |

The full example is in the [README](../README.md#configuration). After changing the config run
`ast-index rebuild`.

## Find code

| Question | Command |
|---|---|
| Where does a feature live | `ast-index explore order cancel refund` — ranked symbols, their source, callers, tests |
| A word in names of files and symbols | `ast-index search Basket` (`--fuzzy`, `--type class`) |
| A class, its FQN | `ast-index class OrderService` |
| Any symbol, with its source | `ast-index symbol calculateTotal --with-content` |
| A file by name | `ast-index file OrderService` |
| The structure of a large file | `ast-index outline src/Order/OrderService.php` — definitions with line ranges |
| What a file imports | `ast-index imports src/pages/cart/Cart.vue` |

Rules that save a wrong answer:

- **Do not guess an FQN.** `ast-index class <ShortName>` shows it; an FQN that is not in the index is answered with
  the classes of that short name.
- **Single quotes around FQNs** (`'App\Order\OrderDto'`); in shell loops `while IFS= read -r fqn` — without `-r`
  the backslashes are eaten.
- **Read the `coverage` line.** It says what the answer checked (tests, configs, PHPDoc, templates); there is no need
  to re-run the same search with rg.

## PHP: refactor safely

Class names are resolved the way PHP resolves them — `namespace`, `use` (aliases, group use), `\FQN`, FQN strings,
PHPDoc types — so classes sharing a short name never mix.

| Before you… | Run | What to check in the answer |
|---|---|---|
| rename a class, change its constructor or API | `ast-index impact 'App\Order\OrderDto'` | every reference with its kind, tests, DI configs (`Foo::class => …`), `.neon/.yaml/.json/.xml` |
| move a class to another namespace | `ast-index move-plan 'App\Order\OrderDto' 'App\Order\Dto'` | new path, namespace line, imports to add and replace, FQN strings, config lines |
| change or remove a method | `ast-index callers 'OrderRepository::save'` | sections **receiver not inferred** and **calls typed as a supertype** — check those by hand |
| see what a method calls | `ast-index callees 'App\Order\OrderService::place' --depth 3` | receivers' types, where each target is declared, implementations of interface methods |
| trust a class is used only where you think | `ast-index usages 'App\Order\OrderDto' --no-tests` | namesakes in other namespaces are not mixed in |
| touch an interface or a base class | `ast-index implementations 'App\Base\Handler'` | subclasses through intermediate classes and aliases |
| see one level of inheritance | `ast-index hierarchy 'App\Base\Handler'` | parents and children as FQNs |
| change a constant or an enum case | `ast-index usages 'App\Cache\CacheEnum::LIST'` | uses through subclasses, `self::`/`static::`, PHPDoc mentions apart |
| read what an enum gives per case | `ast-index enum 'App\Cache\CacheEnum' --method ttl` | values per case, constants and arithmetic folded |
| delete dead classes | `ast-index unused-symbols --module src/Order/ --export-only --limit 500` | **Registered in DI config only** — registered, never requested |
| find copy-pasted classes | `ast-index duplicates --path src/` | `UNUSED` copies — removal candidates; `[same FQN]` — two copies of one class |
| review a branch's API changes | `ast-index changed --symbols --base origin/main` | public methods and constructor parameters added, removed, changed — from git, no checkout |

`ast-index changed` without `--symbols` lists the files the branch changed.

## Bitrix: routes, events, registries, the core

| Question | Command |
|---|---|
| Routes and component actions, with the prefilters in effect (CSRF, auth) | `ast-index routes` (`--kind route\|component`, `--path local/modules/x`) |
| Event subscriptions and their handlers | `ast-index events --event OnSaleOrder` (`--module <path>`, `--no-tests`) |
| Registries of descriptor objects, as data | `ast-index new 'App\Action\Descriptor'` — every `new` with its arguments folded |
| Core or vendor code | `ast-index class CIBlockElement`, then `ast-index outline <its file>` |

`routes` builds prefilters the way the core does: filter attributes or `configureActions()` over
`getDefaultPreFilters()` up the parents, the implicit CSRF filter, `-prefilters` and `+prefilters`; each filter says
where it comes from.

Core and vendor directories listed under `external:` are found by `class`, `symbol`, `file`, `outline`,
`hierarchy`; `implementations` and `impact` follow inheritance through them. `search`, `usages`, `unused-symbols`,
`duplicates` leave them out unless `--external`. Do not grep `bitrix/` or `vendor/` for a class.

## JavaScript, TypeScript, Vue

Imports are resolved to module files as bundlers do: relative paths, `paths` of the nearest `tsconfig`/`jsconfig`,
`js_aliases`, extensions, `index.*`. An export is named `path#name` (`#default` for the default export).

| Before you… | Run |
|---|---|
| rename or change an export | `ast-index impact 'src/shared/price.js#formatPrice'` — imports, uses in code, TS types and Vue templates, re-export chains, mocks, tests |
| see who imports a module | `ast-index impact 'src/ui/Popup.vue'` |
| move or rename a file | `ast-index move-plan 'src/ui/Popup.vue' 'src/ui/popup/'` — specifiers per file in the style each was written |
| delete dead exports and files | `ast-index unused-symbols --module src/` |
| do not know the module | `ast-index impact formatPrice` — answers when one module exports it, else lists them |

`unresolved:` in an answer lists specifiers that did not resolve but may point to the module — check them by hand.
`usages <name>` without a path is a short-name match.

## Frontend to backend: who calls an endpoint

```bash
ast-index http-calls --path src/modules/orders
ast-index http-calls --format json | jq '.calls[] | select(.url // "" | startswith("/api/v1/orders/"))'
```

Every backend call of JS/TS/Vue modules and of inline scripts of PHP templates: `fetch`, axios and its instances,
wrappers, client objects of factories (`createClient().get(url)`), jQuery, `BX.ajax`, component actions — with the
method, the address (constants, maps and helpers folded), the keys sent and the keys read from the answer, plus the
`window.X` data a template hands to scripts. A call through a wrapper is shown where the wrapper is called, with
the address that line passes.

Together with `routes`: `http-calls` says which pages call a route and with what method, `routes` which prefilters
guard it — for example, a changing method called from the site whose route has no CSRF filter.

Details — what is recognised, how addresses fold, how to check a pattern: [HOWTO http-calls](http-calls.md).

## When rg is the better tool

The index wins when the answer spans files: relations, namesakes, hidden directories, tests, dead code. Use `rg`
when one literal string answers:

- a static call of a legacy class (`rg 'CSaleOrder::Update'` — exact and short);
- text that is not code: messages, SQL, config values, commented-out code, docs;
- names built at run time (`$prefix . 'Handler'`, ``import(`./${name}.js`)``), CommonJS `module.exports`;
- files the index leaves out (`exclude`, gitignored directories not under `external`).

"How does X work" in one file you know — just read it; `outline` gives the line numbers.

## Agents and scripts

- **Claude Code.** The plugin puts short rules into the context at session start (only where an index exists) and
  adds a note when a `grep`/`rg` looks for a code symbol. Nothing is written to `CLAUDE.md`.
- **Other agents (Cursor, Codex, Cline…).** The MCP server `ast-index-mcp`: [MCP setup](mcp-setup.md).
- **Scripts.** `--format json` on any command. Raw SQL over the index: `ast-index schema`, then
  `ast-index query 'SELECT …'`; `ast-index db-path` prints the database file.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| "Index not found" | `ast-index rebuild` in the project root |
| An answer misses a recent change | `ast-index watch-status`; `ast-index update` |
| Fewer results than expected | Run from the project root: from a subdirectory the output is limited to it |
| A class is "not found" by FQN | The FQN is guessed: `ast-index class <ShortName>`; quotes around the FQN |
| Framework class not found | Its directory is not under `external:` in `.ast-index.yaml` |
| JS imports through an alias are `unresolved` | Add the prefix to `js_aliases` |
| A hidden directory's code is missing | Add it to `include_hidden`, then `rebuild` |
| Answers stay old after an ast-index upgrade | A watcher still runs the old binary: stop it (`pkill -f '^ast-index watch'`); the next session starts it again, or run `ast-index watch &` |
