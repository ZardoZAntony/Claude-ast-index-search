# Commits

## Format

A commit message is **one imperative line**. Nothing else. No body, no
footer, no trailers, no emoji, no issue numbers unless the user asked for
them.

Good:

```
Resolve extra-root paths in search output
Add MCP server crate
Gate rebuild/update timing output behind --verbose
Fix suffix_array build on musl
Bump to v3.38.1
```

Bad:

```
✨ Add Python support (#123)

This commit adds support for Python files including classes,
functions, and imports.

Co-Authored-By: Claude <noreply@anthropic.com>
```

Rationale: the repo's history stays skim-readable; the PR description is
where prose lives; trailers are noise.

## Staging

Always stage specific files, never `git add -A` or `git add .` — that's how
`.env`, editor backups, and local scratch files sneak into commits.

```bash
git add src/commands/index.rs tests/path_resolver_tests.rs
git commit -m "Resolve extra-root paths in search output"
```

## Never mention in commits, comments, or changelog

Internal infrastructure (the name of any private monorepo, VCS, CI system,
or telemetry service). Use generic terms — "monorepo", "VCS", "CI" — if
context demands it. This repo is public.

## Never, without an explicit ask from the user

- `git push --force` / `git push -f` (and never to `main`/`master`).
- `git reset --hard`.
- `git branch -D`, `git tag -d`, `git push --delete`.
- `git commit --amend` on a commit that was already pushed.
- `git add -A` / `git add .`.
- Editing `git config`.
- Skipping hooks (`--no-verify`).

If a tag already points at a released commit and you need another change,
cut a new patch version — never move the tag.

## Committing a fix and a release together

**Always two steps.**

```bash
# 1. The fix, committed on its own.
git add src/…
git commit -m "Fix X when Y"

# 2. Release on a clean tree.
scripts/bump.sh 3.55.1-php.3 --notes notes.md   # commit "Bump to v…", tag, push, GitHub release
```

`bump.sh` refuses a dirty tree and stages only the version files, so a fix is never absorbed into the bump
commit. See `release.md`.

## Never commit without being asked

The user is in the loop. When work is done, report what changed and let
them say "commit it" — then do the commit. Do not pre-emptively commit.

## Push right after committing

Once the user has approved the commit, push it in the same turn — don't
leave local commits dangling, don't wait for a separate "push" prompt. A
local-only commit is work the user can't see on their other machines,
can't share, and can't recover if the laptop fries.

```bash
git push origin <branch>
```

Exceptions that still need an explicit go-ahead:

- Anything involving `--force` / `-f`, even on a personal branch.
- Pushing a tag that alters a published release (don't — cut a new patch
  instead; see `release.md`).
- Pushing to a protected branch you don't normally push to.

If the push fails (rejected non-fast-forward, hook rejection), stop and
report; don't retry with `--force` to "fix" it.
