# JavaScript, TypeScript and Vue

Indexed files: `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`, `.mts`, `.cts`, `.vue` (`<script>`, `<script setup>` and
the `<template>`). Minified files (`*.min.js`, bundles whose lines run to thousands of characters) are not indexed.

## Module-precise commands

An export is named `path#name` (`#default` for the default export; the path is relative to the project root, with
or without extension, or a directory with `index.*`). A module is named by its path.

| Task | Command |
|---|---|
| Rename an export, change its signature or type | `ast-index impact 'src/shared/price.js#formatPrice'` |
| Only the places that take it (no definition, no uses in its own file) | `ast-index usages 'src/shared/price.js#formatPrice'` |
| Every reference to a module file | `ast-index impact 'src/ui/Popup.vue'` |
| Move or rename a file | `ast-index move-plan 'src/ui/Popup.vue' 'src/ui/popup/'` |
| Dead exports and files nothing references | `ast-index unused-symbols --module src/` |
| Short name, when you do not know the module | `ast-index impact formatPrice` — answers if one module exports it, else lists them |

- `impact 'path#name'` lists the definition (and the declaration line for `export { a as b }` and
  `export default x`) and every reference with its kind: `import` (the line of the imported name), `use` (the
  imported name in code, TS types, Vue template interpolations and tags — PascalCase or kebab-case), `namespace`
  (`import * as ns` when `ns.name` is used), `reexport` (chains through barrels are followed; `via` shows the
  re-exporting module), `jsdoc` (`import('…').Name`), `mock` (`vi.mock`/`jest.mock` of the module — update the
  factory on rename), `dynamic`/`glob` (take the whole module), `local` (uses in the defining file). Tests are
  marked. Above 60 references a per-file summary is printed; `--full` lists every line.
- `impact 'path'` — every import, re-export, `import()`, `require()`, mock, glob and JSDoc type of the file, and
  each export with its number of users.
- `move-plan` — for every file the old and new specifier in the style it was written (relative stays relative,
  `@/` stays `@/`, extension and directory `index` kept), the moved file's own specifiers recomputed from the new
  place, glob patterns to re-check, and a warning when the file moves under another tsconfig/jsconfig.
- `unused-symbols` — exports nobody imports (`used in its file: drop export` when the name is still used
  locally), exports only tests import, and modules nothing references (entry points or dead files). Imports from
  tests, re-exports, JSDoc types, `import()` and globs count; a namespace import, `import()` or a glob takes the whole
  module; mocks do not count. Tests and tool configs (`*.config.js`) are not reported as dead files; entry points
  go to `unused_ignore` in `.ast-index.yaml`. The `coverage (JS/TS):` line says this — no need to re-check with rg.
- Specifiers that did not resolve but end with the module's name are listed under `unresolved:` — check those
  by hand.

## Resolution

Relative paths; `js_aliases` from `.ast-index.yaml` (aliases a bundler or test runner config adds, e.g. vitest
`resolve.alias`); `paths` of the nearest `tsconfig.json`/`jsconfig.json` up from the importing file (with
`baseUrl`, `extends`, solution-style `references`); `/src/...` from the nearest `package.json`; extensions in the
order `.js .mjs .cjs .jsx .ts .mts .cts .tsx .vue`, `index.*`, and `./x.js` for `x.ts`; a `?query` suffix is ignored.
A bare specifier (`vue`, `@vue/test-utils`) is a package.

## Still use rg for

Specifiers built at run time (``import(`./${name}.js`)``), CommonJS `module.exports`, files excluded from the
index, plain text, and a unique literal you only need to locate. For renames, moves, importers and dead exports
the index is the complete answer: in a measured rename of a component rg missed two tests that imported it.

## Other commands

`ast-index outline <file>` (structure with lines), `ast-index imports <file>`, `ast-index symbol <Name>`,
`ast-index explore <words>`. `usages <name>` without a path matches identifiers by short name and ends with a note
naming the modules that export it.
