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
| Direct parents and children of one class | `ast-index hierarchy 'App\Base\Version'` |
| Callers of a method through a type and its subtypes | `ast-index callers 'CacheInvalidatorInterface::invalidate'` |
| What a method calls, as a tree with receiver types | `ast-index callees 'App\Event\Bridge::onSaved' --depth 3` |
| Bitrix event subscriptions and their handlers | `ast-index events [--module local/modules/x] [--event OnSale]` |
| Bitrix routes and component actions with their prefilters | `ast-index routes [--kind route\|component] [--path local/modules/x]` |
| Every `new Foo(...)` with its arguments as values | `ast-index new 'App\Dto\Descriptor'` |
| Enum cases and what `ttl()` returns per case | `ast-index enum 'App\Cache\CacheEnum' --method ttl` |
| Where one constant or enum case is used | `ast-index usages 'App\Cache\CacheEnum::LIST' [--no-tests]` |
| Dead classes in a directory | `ast-index unused-symbols --module src/Order/ --export-only` |
| Copy-pasted classes | `ast-index duplicates --path src/` |
| How a branch changed the API of PHP classes | `ast-index changed --symbols --base origin/master [--head <ref>]` |

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
- **`hierarchy <FQN>`** shows one level each way: what the class extends and implements, as
  FQNs, and the classes whose `extends`/`implements` resolves to it. By a short name it mixes in
  the parents and children of every class of that name.
- **`callers Type::method`** infers the receiver of each call (`$this`, `self::`,
  `parent::`, `$this->prop` from typed/promoted properties or `@var`, `$var` from typed
  parameters, `@var`, `new`, `get(Foo::class)` within the same function, `(new Foo)->`,
  `Foo::`). A variable reassigned or bound by `foreach` on the way is reported as not
  inferred rather than guessed. Output: declarations (including anonymous classes), calls on
  the type or its subtypes, calls typed as a supertype that declares the method (these may
  dispatch to the type at run time), calls whose receiver could not be inferred (check these
  by hand), and same-named methods of unrelated types (excluded).
- **`callees Type::method`** is the other direction: the calls the method makes, read from
  its syntax tree (a call over several lines is one call at the line it starts on), each with
  the type its receiver resolves to and what stated it (`this`, `self`, `parent`, `class`,
  `new`, `property` — typed, promoted or `@var`, `param`, `@var`, `instanceof` — inside the
  `if` or after a guard that returns, `return` — the declared return type of a call on a known
  type, `template` — `@return T` with `@param class-string<T>`, as in `getHandler(Foo::class)`),
  where the method is declared (`declared_in` when inherited — a trait method is read with
  `$this` as the class using the trait, `declared_at`), class constants
  and enum cases passed as arguments (`constants`), `Foo::class` arguments (`classes`). An
  abstract or interface method lists its `implementations`; `--implementations` expands them
  too. `--depth N` (default 2) sets the levels; `--stop <Class|Class::method|Ns\>` keeps a
  branch closed (repeatable); a method met again is marked `expanded_above`. A receiver whose
  type nothing states is `call: null` with the receiver as written — check those by hand.
- **`events`** lists Bitrix event subscriptions — `addEventHandler`, `addEventHandlerCompatible`,
  `registerEventHandler(Compatible)`, legacy `AddEventHandler()` and `RegisterModuleDependences()`,
  a call over several lines at the line it starts on: module, event names (concatenation, class
  constants and a `foreach` over a literal array around the call are folded — one entry per
  handler with all its events), the handler (`[Foo::class, 'm']`, `['Ns\\Foo', 'm']`,
  `[$this, 'm']`, `'Foo::m'`, a function, a closure) with `handler_at` — the lines of its body,
  inherited ones too. A callback built at run time is `dynamic`, with its source.
- **`routes`** lists routes of the routing files (`return function (RoutingConfigurator
  $routes) {…}` — `get`/`post`/…/`any`/`match`, `prefix(…)->group(…)` prefixes joined, `name`,
  `methods` modifiers) and component actions (`…Action` methods of `components/*/class.php`,
  mode `class`, and `ajax.php`, mode `ajax`, with `vendor:name` under `local|bitrix/components`).
  Each entry has the handler (`[Ctrl::class, 'add']` and `'addAction'` both run `addAction`),
  `handler_at`, the method's `attributes` and the class's (`class_attributes`, parents too) as
  written, and `prefilters` the way `Controller::buildFilters()` builds them: the action's
  configuration comes from Bitrix filter attributes on the method
  (`#[Rule\Prefilters([...])]`, `#[Rule\Csrf(type: FilterType::DisablePrefilter)]`, …) or else
  from `configureActions()`; its `prefilters` replace the defaults — `getDefaultPreFilters()` of the first
  class up the parents that declares it, `parent::getDefaultPreFilters()` and `array_merge`
  followed, the core's own defaults when the chain leaves the index or for a component class —
  a CSRF filter added when POST is allowed without one (`implicit`), then `-prefilters` removed
  (subclasses of a removed filter too, as `is_a()` does) and `+prefilters` appended. Each filter
  says where it comes from (`from`). Configuration that `onBuildActions` event handlers add is
  not seen. Descriptor-based
  component actions are not methods: read them with `new <Descriptor FQN>`.
- **`new <FQN>`** lists every `new Foo(...)` with its arguments folded to values — strings,
  numbers, arrays, `Bar::class` as an FQN, class constants and enum cases (`{"case": …}`),
  nested `new` (`{"new": …, "args": […], "named": {…}}`), a variable assigned once before;
  what does not fold stays `{"expr": "<source>"}`. Registries made of objects — descriptors,
  manifests, subscription lists — read as data.
- **`enum <FQN>`** lists the cases with their values; each `--method` adds what the method
  returns per case — for a body `return match ($this) {…}` (several cases per arm, `default`)
  or a single `return`, with constants (`self::HOUR`, other classes'), arithmetic (`60 * 60`),
  concatenation and `$this->value` folded, and the line of the arm. What does not fold
  (`'tag_' . $code`) comes as `expr`.
- **`usages Class::MEMBER`** finds a constant or an enum case of that class only — a namesake
  enum in another namespace, imported under an alias, is not mixed in. It starts from the class
  that declares the member and follows the subclasses that inherit it (`Child::LIMIT`,
  `self::`/`static::` in a subclass, `parent::`), skipping those that redeclare it (`subtypes`). Uses inside the class
  itself are tagged `self`, tests `test` (`--no-tests` drops them); PHPDoc mentions
  (`{@see Class::MEMBER}`) are listed apart. `--no-tests` also works for `usages <FQN>`.
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
- **`changed --symbols`** reads both versions of every PHP file the branch touched from git
  (merge base and `--head`, HEAD by default — no checkout, no index) and compares them by
  syntax tree: per class, public methods added, removed or changed (signature before and
  after; `body` when only the body changed), the constructor with added (`required` when it
  has no default), removed and changed parameters, and the class header (`extends`,
  `implements`, modifiers). Interfaces and abstract classes list protected methods too;
  narrowing public to protected or private is `removed`. A renamed file whose class changed
  namespace is `moved` (`moved_from`). Constants (public; protected too in interfaces and
  abstract classes) and enum cases come under `constants`: added, removed, or `changed` value.
  Layout, comments, phpDoc and attributes are no change; a default value is; class names compare
  by FQN, so an import alias is no change.
- **`usages <ShortName>`** notes when several classes share the name; pass the FQN instead.

## Framework and vendor code

List the directories of code the project uses but does not own under `external:` in `.ast-index.yaml` (they may be
gitignored): `bitrix/modules/main/lib`, `local/vendor/symfony`… Their classes are found by `class`/`symbol`/`file`,
`outline` shows their methods, `hierarchy` and `implementations` follow inheritance through them (external
subclasses are hidden with a count), `impact` and `callers 'Type::method'` show their definitions and declarations.
Only symbols and inheritance are stored for them — `search`, `usages`, `unused-symbols`, `duplicates` and grep leave
them out unless `--external`.

## When to use grep instead

- One literal string answers: a static call of a legacy class (`rg 'CSaleOrder::Update'` — exact and short; `callers`
  adds calls it cannot type), a class name you only need to locate (`class` is no faster).
- Text that is not PHP code or config: templates in other languages, docs, JS.
- Dynamically built class names (`$class = $prefix . 'Handler'`) and method names.
- Receivers the inference cannot follow (factory return types, array elements) — `callers` lists them with a
  summary by receiver; decide by hand.
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
