# PHP Commands Reference

ast-index resolves PHP class names the way PHP does — per file, from `namespace` and `use`
imports (aliases and group use included), `\Fully\Qualified` names, FQN strings in code
(`'App\\Handler'`) and PHPDoc types (`@var`, `@param`, `@return`, `@throws`, `@extends`,
`@implements`, `@method`, `@see`, …). Every class reference keeps its fully qualified name
(FQN), so classes that share a short name are never mixed up. Class names are matched
case-insensitively, as in PHP.

## Answer refactoring questions in one call

| Task | Command |
|------|---------|
| Everything a rename or signature change touches | `ast-index impact 'App\Order\OrderDto'` |
| Move a class to another namespace | `ast-index move-plan 'App\Order\OrderDto' 'App\Order\Dto'` |
| Usages of one class (not its namesakes) | `ast-index usages 'App\Order\OrderDto'` |
| Callers of a method through a type and its subtypes | `ast-index callers 'CacheInvalidatorInterface::invalidate'` |
| Dead classes in a directory | `ast-index unused-symbols --module src/Order/` |
| Copy-pasted classes | `ast-index duplicates --path src/` |

- **`impact`** lists the definition and every reference with its kind (`import`, `type`,
  `new`, `static`, `::class`, `phpdoc`, `attribute`, `string`, `check`, `inheritance`), marks
  tests, and scans config files (`.neon`, `.yaml`, `.yml`, `.json`, `.xml`) for the FQN and
  the class file path. The `coverage` line says what was checked — no need to re-grep it.
- **`move-plan`** prints the edits per file: the new path (PSR-4), the namespace line, `use`
  lines to add for same-namespace classes the moved class used without import, imports to
  replace, same-namespace users that now need an import, FQN strings, config lines.
- **`callers Type::method`** infers the receiver of each call (`$this`, `$this->prop` from
  typed/promoted properties or `@var`, `$var` from typed parameters, `@var`, `new`,
  `get(Foo::class)`, `(new Foo)->`, `Foo::`). Output: declarations (including anonymous
  classes), calls on the type or its subtypes, calls whose receiver could not be inferred
  (check these by hand), and same-named methods of unrelated types (excluded).
- **`unused-symbols`** checks PHP classes by FQN, so a dead class with a live namesake is
  reported. A reference counts wherever it is: code, DI config, PHPDoc, tests. Classes loaded
  by framework convention (module installers, controllers found by name) look unused.
- **`duplicates`** compares bodies of same-named classes (token similarity, default ≥ 0.75)
  and shows how many references each copy has — `UNUSED` copies are removal candidates.
  Pairs where one path is under `/Contracts/` are tagged `[contract mirror]`.

## When to use grep instead

- Text that is not PHP code or config: templates in other languages, docs, JS.
- Dynamically built class names (`$class = $prefix . 'Handler'`).
- Files excluded from the index (`.ast-index.yaml` `exclude`, gitignored directories).

## Project config

Hidden directories hold real code in some PHP frameworks (Bitrix component templates in
`.default/`, module wiring in `.settings.php`); list them in `include_hidden`. To keep the
config out of the repository, put it in `.git/ast-index.yaml`.

```yaml
include_hidden:
  - .default
  - .settings.php
exclude:
  - "*.css"
  - vendor/
```
