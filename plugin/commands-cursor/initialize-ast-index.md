---
name: initialize-ast-index
description: Initialize ast-index guidance for a Cursor project.
---

# Initialize ast-index for Cursor

Set up a project so Cursor agents consistently use `ast-index` for structural
code search.

## Steps

1. Verify the CLI is available:

```bash
ast-index version
```

If it is missing, install it:

```bash
cargo install --locked --git https://github.com/ZardoZAntony/Claude-ast-index-search ast-index
```

2. Create a Cursor project rule:

```bash
mkdir -p .cursor/rules
```

Create or update `.cursor/rules/ast-index.mdc`:

```markdown
---
description: Use ast-index for fast structural code search before broad text search.
alwaysApply: true
---

Use `ast-index` first for codebase navigation tasks: files, symbols, classes,
usages, implementations, callers, outlines, dependencies, and project maps.

Run `ast-index stats` before the first query. If no usable index exists, run
`ast-index rebuild` from the project root. In large repositories, prefer the
smallest relevant root or a scoped rebuild with `--include`.

Do not run grep or broad text search after `ast-index` returns sufficient
structural results. Fall back to text search only for regex searches, string
literals, comments, or when `ast-index` returns no useful result.
```

3. Build or refresh the index:

```bash
ast-index rebuild
```

For large repositories, scope the first build:

```bash
ast-index rebuild --include path/to/module
```

4. Verify:

```bash
ast-index stats
ast-index search "ViewModel" --limit 5
ast-index map --limit 10
```

Report the indexed file and symbol counts from `ast-index stats`, plus one
successful search result.
