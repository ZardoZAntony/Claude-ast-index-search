---
name: ast-index
description: >-
  Structural code index for PHP, JavaScript, TypeScript and Vue projects (ast-index CLI): who uses or calls a
  class, function or method, what a rename, move, delete or signature change touches, dead code, duplicate
  classes, the outline of a large file. Use it BEFORE grep/rg for these tasks — it resolves PHP namespaces and
  imports, sees PHPDoc and tests, and answers in one call. Phrases: "find usages", "who calls", "rename/move
  class", "change signature", "remove method", "unused/dead code", "duplicates", "outline"; «где используется»,
  «кто вызывает», «переименовать/перенести класс», «сменить сигнатуру», «убрать поле/метод/параметр»,
  «удалить класс», «мёртвый код», «неиспользуемые классы», «дубли», «структура файла», «найти класс/функцию».
user-invocable: false
---

# ast-index — code index for PHP, JS/TS and Vue

`ast-index` keeps a SQLite index of symbols and references of the project and answers structural questions in
milliseconds. Run it from the project root. The plugin keeps the index fresh (a watcher per project plus a hook
after edits); after a bulk shell change (`git switch`, a code generator) run `ast-index update` once.

## When to reach for it

| Before you… | Run |
|---|---|
| rename a PHP class, change its constructor or public API | `ast-index impact '<FQN>'` |
| move a PHP class to another namespace | `ast-index move-plan '<FQN>' '<New\Namespace>'` |
| rename or change a JS/TS export | `ast-index impact '<file>#<name>'` (`#default`; a short name if one module exports it) |
| move or rename a JS/TS/Vue file | `ast-index move-plan '<file>' '<new path or dir/>'` |
| find who imports a JS/TS/Vue module | `ast-index impact '<file>'` |
| change or remove a method (signature, parameter, return type) | `ast-index callers '<Type>::<method>'` |
| trust that a class is used only where you think | `ast-index usages '<FQN>'` |
| touch an interface or a base class | `ast-index implementations '<FQN>'` |
| delete "dead" classes or JS exports and files | `ast-index unused-symbols --module <dir>/ --export-only --limit 500` |
| suspect copy-pasted classes | `ast-index duplicates --path <dir>/` |
| read a large file | `ast-index outline <file>` |
| read framework or vendor code (Bitrix core, Symfony…) | `ast-index class <Name>`, then `ast-index outline <its file>` |
| look for where a feature lives | `ast-index explore <words>` or `ast-index search <word>` |

Each of these is a complete answer: its `coverage` line says what was checked, so do not re-run the same search
with rg. Do not guess an FQN — `ast-index class <ShortName>` shows it, and an FQN that is not in the index is
answered with the classes of that short name. FQN arguments go in single quotes; in shell loops use
`while IFS= read -r fqn` (without `-r` the backslashes are eaten and the command silently looks for another
name).

## PHP

Class names are resolved per file from `namespace` and `use` (aliases, group use), `\FQN`, FQN strings in code
and PHPDoc types, so namesakes in different namespaces never mix. Strings, heredoc SQL, comments and inline HTML
are not taken for code.

- `impact` — the definition and every reference with its kind (`import`, `type`, `new`, `static`, `::class`,
  `phpdoc`, `attribute`, `inheritance`, `check`, `string`), tests marked, plus config files (`.neon`, `.yaml`,
  `.json`, `.xml`). Above 60 references it prints a per-file summary; `--full` gives every line. DI configs in PHP
  (`Foo::class => [...]` in `di/`, `.settings.php`) and tests are part of the answer — a separate grep of `di/` or
  the tests for the class name adds nothing; grep there only for string values.
- `move-plan` — edits per file: new path, namespace line, imports to add and replace, FQN strings, config lines.
- `callers 'Type::method'` — declarations (anonymous classes too), calls on the type and its subtypes. Check by
  hand the sections **"receiver not inferred"** (foreach, reassignment, factory results) and **"calls typed as
  a supertype"** (may dispatch to your type at run time).
- `unused-symbols` — dead classes by FQN. Framework entry points (installers, `*Action`, ORM overrides) are
  excluded by the project config; anything else the framework calls by name — check by hand. Section
  **"Registered in DI config only"** — classes registered but never requested.
- `duplicates` — `[mirror]` are deliberate copies from `duplicate_mirrors`, `[same FQN]` two copies of one class
  (usually a forgotten one), `UNUSED` — removal candidate.

## JavaScript, TypeScript, Vue

Imports are resolved to module files the way bundlers do (relative paths, `paths` of the nearest
tsconfig/jsconfig, `js_aliases` from `.ast-index.yaml`, extensions, `index.*`), so answers are per module, not
per short name.

- `impact '<file>#<name>'` — definition, imports (the line of the imported name), uses in code, TS types and Vue
  templates (tags too), re-export chains through barrels, JSDoc `import()` types, mocks and `import()` of the
  module, uses in the defining file; tests marked. `usages '<file>#<name>'` — the same without the definition side.
- `impact '<file>'` — every reference to the file and each export with its number of users.
- `move-plan '<file>' '<new path>'` — specifiers to rewrite per file, in their own style, plus the moved file's
  own relative imports.
- `unused-symbols --module <dir>/` — exports nobody imports (`drop export` when used in their file), exports only
  tests import, modules nothing references; imports from tests, templates, `import()`, globs and JSDoc types are
  counted (see its `coverage` line); entry points go to `unused_ignore`.
- `usages <name>` without a path is still a short-name match; its note names the modules that export the name.
- `unresolved:` in the answer lists specifiers that did not resolve but may point to the module — check them.

## Framework and vendor code

Directories listed under `external:` in `.ast-index.yaml` (framework core, vendor packages — usually gitignored) are
indexed for their definitions: `class`, `symbol`, `file`, `outline`, `hierarchy` find them, `impact`/`callers` show
their definitions and declarations, `implementations` follows inheritance through them. `search`, `explore`,
`usages`, `refs`, `unused-symbols`, `duplicates` and grep-based commands leave them out; `--external` brings them
in. `implementations` prints how many external subclasses it hid. Do not grep `bitrix/` or `vendor/` for a class:
`ast-index class <Name>` gives the file, `outline` its methods with lines.

## Still use rg for

Text that is not code (docs, messages, templates in other languages), commented-out code, dynamically built
class or method names (`$prefix . 'Handler'`) and module specifiers (``import(`./${name}.js`)``), CommonJS
`module.exports`, files excluded from the index (`vendor/`, styles, generated files).

## Index and config

- `ast-index stats` — whether an index exists and what is in it; `ast-index rebuild` — create it (seconds).
- `.ast-index.yaml` in the project root: `exclude`, `include_hidden` (hidden paths with real code, e.g. Bitrix
  `.default/`, `.settings.php`, `.tests/`), `unused_ignore` / `unused_ignore_names` (entry points the framework
  calls by convention), `duplicate_mirrors`, `js_aliases` (specifier prefixes a bundler or test runner adds beyond
  tsconfig/jsconfig `paths`), `external` (framework and vendor directories indexed for definitions only).

More detail: `references/php-commands.md`, `references/typescript-commands.md`.
