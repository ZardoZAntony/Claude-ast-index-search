# PHP Commands Reference

ast-index resolves PHP class names the way PHP does — per file, from `namespace` and `use`
imports (aliases and group use included), `\Fully\Qualified` names, FQN strings in code
(`'App\\Handler'`) and PHPDoc types (`@var`, `@param`, `@return`, `@throws`, `@extends`,
`@implements`, `@method`, `@see`, …). Every class reference keeps its fully qualified name
(FQN) and its kind of use, so classes that share a short name are never mixed up. Class
names are matched case-insensitively, as in PHP.

The indexer lexes each file, so words inside strings, heredoc SQL, comments and inline HTML
(`?>` closes PHP inside a `//` comment too) are not taken for classes; enum cases, named
arguments, constants and keywords in any letter case (`Throw New …`) are skipped too. A class's
references in its own file count (`new Foo()` in Foo's factory), its declaration does not.
Method names are matched case-insensitively (`GetList` called as `getList`).

## Answer refactoring questions in one call

| Task | Command |
|------|---------|
| Everything a rename or signature change touches | `ast-index impact 'App\Order\OrderDto'` |
| Move a class to another namespace | `ast-index move-plan 'App\Order\OrderDto' 'App\Order\Dto'` |
| Usages of one class (not its namesakes) | `ast-index usages 'App\Order\OrderDto'` |
| Subclasses and implementations, aliases included | `ast-index implementations 'App\Base\Version'` |
| Callers of a method through a type and its subtypes | `ast-index callers 'CacheInvalidatorInterface::invalidate'` |
| Dead classes in a directory | `ast-index unused-symbols --module src/Order/ --export-only` |
| Copy-pasted classes | `ast-index duplicates --path src/` |

- **`impact`** lists the definition and every reference with its kind (`import`,
  `trait-use`, `type`, `new`, `static`, `::class`, `phpdoc`, `attribute`, `string`, `check`,
  `inheritance`), marks tests, and scans config files (`.neon`, `.yaml`, `.yml`, `.json`,
  `.xml`) for the FQN and the class file path. Above 60 references the text output lists
  files with counts by kind; `--full` or `--format json` gives every line. The `coverage`
  line says what was checked — no need to re-grep it.
- **`move-plan`** prints the edits per file: the new path (PSR-4, case-insensitive, lower-case
  directories kept lower-case), the namespace line, `use` lines to add for same-namespace
  classes and traits the moved class used without import, imports to replace, same-namespace
  users that now need an import, FQN strings, config lines. It warns when the target FQN is
  already taken or the class is defined more than once.
- **`implementations <FQN>`** follows `extends`/`implements` under any alias the parent is
  imported with, transitively. It also works for types outside the index (vendor, framework
  core, generated ORM classes) that code references by FQN.
- **`callers Type::method`** infers the receiver of each call (`$this`, `self::`,
  `parent::`, `$this->prop` from typed/promoted properties or `@var`, `$var` from typed
  parameters, `@var`, `new`, `get(Foo::class)` within the same function, `(new Foo)->`,
  `Foo::`). A variable reassigned or bound by `foreach` on the way is reported as not
  inferred rather than guessed. Output: declarations (including anonymous classes), calls on
  the type or its subtypes, calls typed as a supertype that declares the method (these may
  dispatch to the type at run time), calls whose receiver could not be inferred (check these
  by hand), and same-named methods of unrelated types (excluded).
- **`unused-symbols`** checks PHP classes by FQN, so a dead class with a live namesake is
  reported. A reference counts wherever it is: code, PHPDoc, tests, callables
  (`[Foo::class, 'handle']`, `[$this, 'handle']`, `array('Foo', 'handle')`, `'Foo::handle'`). Classes referenced only where a DI config
  registers them (`Foo::class => [...]` in `di/` or `.settings.php`), never requested, are
  listed separately. Magic methods are skipped. Entry points a framework calls by convention
  go to `unused_ignore` (files) and `unused_ignore_names` (method names) in the config.
- **`duplicates`** compares bodies of same-named classes (token similarity, default ≥ 0.75)
  and shows how many references each copy has — `UNUSED` copies are removal candidates.
  Pairs where a path contains one of `duplicate_mirrors` from the config are deliberate copies,
  tagged `[mirror]` and listed last; copies that declare the same FQN are tagged `[same FQN]` —
  their references cannot be told apart.
- **`usages <ShortName>`** notes when several classes share the name; pass the FQN instead.

## Framework and vendor code

List the directories of code the project uses but does not own under `external:` in `.ast-index.yaml` (they may be
gitignored): `bitrix/modules/main/lib`, `local/vendor/symfony`… Their classes are found by `class`/`symbol`/`file`,
`outline` shows their methods, `hierarchy` and `implementations` follow inheritance through them (external
subclasses are hidden with a count), `impact` and `callers 'Type::method'` show their definitions and declarations.
Only symbols and inheritance are stored for them — `search`, `usages`, `unused-symbols`, `duplicates` and grep leave
them out unless `--external`.

## When to use grep instead

- Text that is not PHP code or config: templates in other languages, docs, JS.
- Dynamically built class names (`$class = $prefix . 'Handler'`) and method names.
- Receivers the inference cannot follow (factory return types, array elements).
- Files excluded from the index (`.ast-index.yaml` `exclude`, gitignored directories not listed under `external`).

## Project config

Hidden directories hold real code in some PHP frameworks (Bitrix component templates in
`.default/`, module wiring in `.settings.php`); list them in `include_hidden` in the
project's `.ast-index.yaml`.

```yaml
include_hidden:
  - .default
  - .settings.php
exclude:
  - "*.css"
  - vendor/
unused_ignore:            # files whose symbols the framework calls by convention
  - "**/install/index.php"
unused_ignore_names:      # method names the framework calls; `*` is a wildcard
  - "*Action"
  - getObjectClass
duplicate_mirrors:        # path fragments of deliberate copies
  - /Contracts/
```
