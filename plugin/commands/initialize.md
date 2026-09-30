---
name: initialize
description: Set up ast-index for a PHP, JavaScript/TypeScript or Vue project — config, first index, check
---

# Initialize ast-index

Set up the index for the current project. The plugin itself gives the agent the usage rules at session start
(only in projects that have an index), so nothing goes into `CLAUDE.md` or `.claude/rules/`.

## 1. Binary

```bash
ast-index version
```

If it is missing, tell the user to install the fork's release and stop until it exists:

```bash
curl -fsSL https://github.com/ZardoZAntony/Claude-ast-index-search/releases/latest/download/ast-index-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin
# or: cargo install --locked --git https://github.com/ZardoZAntony/Claude-ast-index-search ast-index
```

## 2. `.ast-index.yaml` in the project root

Gitignored paths are skipped already. Look at the repository and propose to the user only what applies:

- `exclude` — code that is not the project's: vendored libraries outside `.gitignore` (`vendor/`, copies of
  jQuery/Vue/Swiper in the tree), styles (`"*.css"`, `"*.scss"`, `"*.less"` — selectors are not code symbols),
  generated IDE helpers (`orm_annotation.php`), dated copies of old code.
- `include_hidden` — hidden paths that hold real code: Bitrix component templates `.default`, module wiring
  `.settings.php`, tests in `.tests`.
- `unused_ignore` / `unused_ignore_names` — what the framework calls by convention, so `unused-symbols` does not
  report it: module installers (`"**/install/index.php"`), controller actions (`"*Action"`), ORM overrides
  (`getMap`, `getTableName`, `getObjectClass`, `getCollectionClass`).
- `duplicate_mirrors` — path fragments of deliberate copies (DTO contracts mirrored between modules).
- `js_aliases` — JS/TS specifier prefixes that `vite.config`/`vitest.config` `resolve.alias` adds beyond
  tsconfig/jsconfig `paths` (`"@frontend-ui/": "frontend/src/ui/"`). Check with `ast-index impact '<file>'`: its
  `unresolved:` section lists specifiers that did not resolve.
- JS entry points (`main.js`, `entry-client.js`, CLI scripts) into `unused_ignore`, so `unused-symbols` lists only
  files that are really dead.
- `external` — directories of framework and vendor code the project calls (Bitrix `bitrix/modules/<module>/lib`,
  `vendor/symfony`, `vendor/psr`…), so their classes and methods are found without grep. Pick the ones the code
  references most: `ast-index query` over `refs.fqn` not defined in the index shows the namespaces.

Show the file to the user before writing it. Whether it is committed is the user's call.

## 3. Permission for the agent

Suggest `"Bash(ast-index *)"` in `permissions.allow` of `.claude/settings.local.json` (personal). Put it in the
shared `.claude/settings.json` only if the user wants the whole team on ast-index.

## 4. Build and check

```bash
ast-index rebuild
ast-index stats
```

Then one query the user can recognise: `ast-index outline <a large file>` or, in PHP, `ast-index impact '<FQN of
a well-known class>'`. Report the file and symbol counts and the rebuild time.
